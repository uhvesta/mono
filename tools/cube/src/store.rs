use std::fs;
use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};

use crate::app::CubeError;
use crate::metadata::{
    ChangeRecord, RepoRecord, WorkspaceCandidate, WorkspaceHealth, WorkspaceRecord, WorkspaceState,
};
use crate::paths::database_path;

pub struct Store {
    connection: Connection,
}

/// Effective display state of a workspace: combines lease state with health.
/// Used by `cube workspace list --state` to filter on composite status strings
/// that include health info (e.g. `free-dirty`, `free-conflicted`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectiveState {
    /// state=free, health_status IS NULL or 'clean'
    Free,
    /// state=free, health_status='dirty'
    FreeDirty,
    /// state=free, health_status='conflicted'
    FreeConflicted,
    /// state=leased
    Leased,
}

impl EffectiveState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Free => "free",
            Self::FreeDirty => "free-dirty",
            Self::FreeConflicted => "free-conflicted",
            Self::Leased => "leased",
        }
    }

    pub fn from_str(raw: &str) -> Option<Self> {
        match raw {
            "free" => Some(Self::Free),
            "free-dirty" => Some(Self::FreeDirty),
            "free-conflicted" => Some(Self::FreeConflicted),
            "leased" => Some(Self::Leased),
            _ => None,
        }
    }
}

#[derive(Default, Debug)]
pub struct WorkspaceListFilter<'a> {
    pub repo: Option<&'a str>,
    pub workspace_id: Option<&'a str>,
    /// Filter by effective state (combines lease state + health). When set,
    /// `state` is ignored.
    pub effective_state: Option<EffectiveState>,
    pub holder_pattern: Option<&'a str>,
}

/// One row swept by [`Store::expire_stale_leases`]. The lease handler
/// needs the original holder/task/lease_id so it can write an audit
/// trail for the timeline ("`lease.expired_reclaimed`") and tell the
/// reset path that the workspace it just claimed used to belong to
/// someone — so destructive `jj new <main>` should be guarded.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ExpiredLease {
    pub workspace_id: String,
    pub lease_id: String,
    pub holder: Option<String>,
    pub task: Option<String>,
    pub leased_at_epoch_s: Option<i64>,
    pub lease_expires_at_epoch_s: Option<i64>,
}

impl Store {
    pub fn open_default() -> Result<Self, CubeError> {
        let path = database_path()?;
        Self::open_at(path)
    }

    pub fn open_at(path: impl AsRef<Path>) -> Result<Self, CubeError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| CubeError::StateDbIo {
                path: parent.to_path_buf(),
                source: e,
            })?;
        }

        let connection = Connection::open(path).map_err(CubeError::Storage)?;
        let store = Self { connection };
        store.migrate()?;
        Ok(store)
    }

    pub fn upsert_repo(&self, config: &RepoRecord) -> Result<RepoRecord, CubeError> {
        self.connection
            .execute(
                r#"
                INSERT INTO repos (
                    repo,
                    origin,
                    main_branch,
                    workspace_root,
                    workspace_prefix,
                    source_path
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                ON CONFLICT(repo) DO UPDATE SET
                    origin = excluded.origin,
                    main_branch = excluded.main_branch,
                    workspace_root = excluded.workspace_root,
                    workspace_prefix = excluded.workspace_prefix,
                    source_path = excluded.source_path
                "#,
                params![
                    config.repo,
                    config.origin,
                    config.main_branch,
                    config.workspace_root.display().to_string(),
                    config.workspace_prefix,
                    config.source.as_ref().map(|path| path_to_string(path)),
                ],
            )
            .map_err(CubeError::Storage)?;

        self.get_repo(&config.repo)?
            .ok_or_else(|| CubeError::RepoNotFound(config.repo.clone()))
    }

    pub fn get_repo(&self, repo: &str) -> Result<Option<RepoRecord>, CubeError> {
        self.connection
            .query_row(
                r#"
                SELECT repo, origin, main_branch, workspace_root, workspace_prefix, source_path
                FROM repos
                WHERE repo = ?1
                "#,
                params![repo],
                row_to_repo_record,
            )
            .optional()
            .map_err(CubeError::Storage)
    }

    pub fn get_repo_by_origin(&self, origin: &str) -> Result<Option<RepoRecord>, CubeError> {
        self.connection
            .query_row(
                r#"
                SELECT repo, origin, main_branch, workspace_root, workspace_prefix, source_path
                FROM repos
                WHERE origin = ?1
                ORDER BY repo
                LIMIT 1
                "#,
                params![origin],
                row_to_repo_record,
            )
            .optional()
            .map_err(CubeError::Storage)
    }

    pub fn list_repos(&self) -> Result<Vec<RepoRecord>, CubeError> {
        let mut statement = self
            .connection
            .prepare(
                r#"
                SELECT repo, origin, main_branch, workspace_root, workspace_prefix, source_path
                FROM repos
                ORDER BY repo
                "#,
            )
            .map_err(CubeError::Storage)?;
        let rows = statement
            .query_map([], row_to_repo_record)
            .map_err(CubeError::Storage)?;

        rows.collect::<Result<Vec<_>, _>>()
            .map_err(CubeError::Storage)
    }

    pub fn list_workspaces_filtered(
        &self,
        filter: &WorkspaceListFilter<'_>,
    ) -> Result<Vec<WorkspaceRecord>, CubeError> {
        let mut sql = String::from(
            r#"
            SELECT
                repo,
                workspace_id,
                workspace_path,
                state,
                lease_id,
                holder,
                task,
                leased_at_epoch_s,
                lease_expires_at_epoch_s,
                head_commit,
                last_release_reason,
                health_status
            FROM workspaces
            WHERE 1=1
            "#,
        );
        let mut bound: Vec<String> = Vec::new();
        if let Some(repo) = filter.repo {
            sql.push_str(" AND repo = ?");
            bound.push(repo.to_string());
        }
        if let Some(workspace_id) = filter.workspace_id {
            sql.push_str(" AND workspace_id = ?");
            bound.push(workspace_id.to_string());
        }
        if let Some(effective) = filter.effective_state {
            match effective {
                EffectiveState::Free => {
                    sql.push_str(
                        " AND state = ? AND (health_status IS NULL OR health_status = 'clean')",
                    );
                    bound.push(WorkspaceState::Free.as_str().to_string());
                }
                EffectiveState::FreeDirty => {
                    sql.push_str(" AND state = ? AND health_status = 'dirty'");
                    bound.push(WorkspaceState::Free.as_str().to_string());
                }
                EffectiveState::FreeConflicted => {
                    sql.push_str(" AND state = ? AND health_status = 'conflicted'");
                    bound.push(WorkspaceState::Free.as_str().to_string());
                }
                EffectiveState::Leased => {
                    sql.push_str(" AND state = ?");
                    bound.push(WorkspaceState::Leased.as_str().to_string());
                }
            }
        }
        if let Some(holder_pattern) = filter.holder_pattern {
            sql.push_str(" AND holder GLOB ?");
            bound.push(holder_pattern.to_string());
        }
        sql.push_str(" ORDER BY repo, workspace_id");

        let mut statement = self.connection.prepare(&sql).map_err(CubeError::Storage)?;
        let rows = statement
            .query_map(
                rusqlite::params_from_iter(bound.iter()),
                row_to_workspace_record,
            )
            .map_err(CubeError::Storage)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(CubeError::Storage)
    }

    pub fn list_workspaces(&self, repo: &str) -> Result<Vec<WorkspaceRecord>, CubeError> {
        let mut statement = self
            .connection
            .prepare(
                r#"
                SELECT
                    repo,
                    workspace_id,
                    workspace_path,
                    state,
                    lease_id,
                    holder,
                    task,
                    leased_at_epoch_s,
                    lease_expires_at_epoch_s,
                    head_commit,
                    last_release_reason,
                    health_status
                FROM workspaces
                WHERE repo = ?1
                ORDER BY workspace_id
                "#,
            )
            .map_err(CubeError::Storage)?;
        let rows = statement
            .query_map(params![repo], row_to_workspace_record)
            .map_err(CubeError::Storage)?;

        rows.collect::<Result<Vec<_>, _>>()
            .map_err(CubeError::Storage)
    }

    pub fn sync_workspaces(
        &mut self,
        repo: &str,
        candidates: &[WorkspaceCandidate],
    ) -> Result<(), CubeError> {
        let transaction = self.connection.transaction().map_err(CubeError::Storage)?;
        let candidate_ids = candidates
            .iter()
            .map(|candidate| candidate.workspace_id.as_str())
            .collect::<Vec<_>>();
        for candidate in candidates {
            transaction
                .execute(
                    r#"
                    INSERT INTO workspaces (
                        repo,
                        workspace_id,
                        workspace_path,
                        state
                    ) VALUES (?1, ?2, ?3, ?4)
                    ON CONFLICT(repo, workspace_id) DO UPDATE SET
                        workspace_path = excluded.workspace_path
                    "#,
                    params![
                        repo,
                        candidate.workspace_id,
                        candidate.workspace_path.display().to_string(),
                        WorkspaceState::Free.as_str(),
                    ],
                )
                .map_err(CubeError::Storage)?;
        }

        let mut statement = transaction
            .prepare(
                r#"
                SELECT workspace_id
                FROM workspaces
                WHERE repo = ?1 AND state = ?2
                "#,
            )
            .map_err(CubeError::Storage)?;
        let existing = statement
            .query_map(params![repo, WorkspaceState::Free.as_str()], |row| {
                row.get::<_, String>(0)
            })
            .map_err(CubeError::Storage)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(CubeError::Storage)?;
        drop(statement);

        for workspace_id in existing {
            if candidate_ids
                .iter()
                .any(|candidate_id| *candidate_id == workspace_id)
            {
                continue;
            }
            transaction
                .execute(
                    r#"
                    DELETE FROM workspaces
                    WHERE repo = ?1 AND workspace_id = ?2 AND state = ?3
                    "#,
                    params![repo, workspace_id, WorkspaceState::Free.as_str()],
                )
                .map_err(CubeError::Storage)?;
        }
        transaction.commit().map_err(CubeError::Storage)
    }

    pub fn claim_workspace(
        &mut self,
        repo: &str,
        holder: &str,
        task: &str,
        lease_id: &str,
        leased_at_epoch_s: i64,
        lease_expires_at_epoch_s: Option<i64>,
        prefer: Option<&str>,
    ) -> Result<Option<WorkspaceRecord>, CubeError> {
        let transaction = self.connection.transaction().map_err(CubeError::Storage)?;
        let candidate = if let Some(preferred_id) = prefer {
            let preferred = transaction
                .query_row(
                    r#"
                    SELECT workspace_id, workspace_path
                    FROM workspaces
                    WHERE repo = ?1 AND workspace_id = ?2 AND state = ?3
                    "#,
                    params![repo, preferred_id, WorkspaceState::Free.as_str()],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .optional()
                .map_err(CubeError::Storage)?;
            if preferred.is_some() {
                preferred
            } else {
                transaction
                    .query_row(
                        r#"
                        SELECT workspace_id, workspace_path
                        FROM workspaces
                        WHERE repo = ?1 AND state = ?2
                        ORDER BY workspace_id
                        LIMIT 1
                        "#,
                        params![repo, WorkspaceState::Free.as_str()],
                        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                    )
                    .optional()
                    .map_err(CubeError::Storage)?
            }
        } else {
            transaction
                .query_row(
                    r#"
                    SELECT workspace_id, workspace_path
                    FROM workspaces
                    WHERE repo = ?1 AND state = ?2
                    ORDER BY workspace_id
                    LIMIT 1
                    "#,
                    params![repo, WorkspaceState::Free.as_str()],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .optional()
                .map_err(CubeError::Storage)?
        };

        let Some((workspace_id, workspace_path)) = candidate else {
            transaction.rollback().map_err(CubeError::Storage)?;
            return Ok(None);
        };

        transaction
            .execute(
                r#"
                UPDATE workspaces
                SET
                    state = ?1,
                    lease_id = ?2,
                    holder = ?3,
                    task = ?4,
                    leased_at_epoch_s = ?5,
                    lease_expires_at_epoch_s = ?6,
                    head_commit = NULL,
                    last_release_reason = NULL
                WHERE repo = ?7 AND workspace_id = ?8 AND state = ?9
                "#,
                params![
                    WorkspaceState::Leased.as_str(),
                    lease_id,
                    holder,
                    task,
                    leased_at_epoch_s,
                    lease_expires_at_epoch_s,
                    repo,
                    workspace_id,
                    WorkspaceState::Free.as_str(),
                ],
            )
            .map_err(CubeError::Storage)?;

        let claimed = transaction
            .query_row(
                r#"
                SELECT
                    repo,
                    workspace_id,
                    workspace_path,
                    state,
                    lease_id,
                    holder,
                    task,
                    leased_at_epoch_s,
                    lease_expires_at_epoch_s,
                    head_commit,
                    last_release_reason,
                    health_status
                FROM workspaces
                WHERE repo = ?1 AND workspace_id = ?2
                "#,
                params![repo, workspace_id],
                |row| row_to_workspace_record(row),
            )
            .map_err(CubeError::Storage)?;
        transaction.commit().map_err(CubeError::Storage)?;

        debug_assert_eq!(claimed.workspace_path, Path::new(&workspace_path));
        Ok(Some(claimed))
    }

    pub fn update_workspace_head_commit(
        &self,
        lease_id: &str,
        head_commit: Option<&str>,
    ) -> Result<(), CubeError> {
        self.connection
            .execute(
                r#"
                UPDATE workspaces
                SET head_commit = ?2
                WHERE lease_id = ?1
                "#,
                params![lease_id, head_commit],
            )
            .map_err(CubeError::Storage)?;
        Ok(())
    }

    /// Persist the health status of a free workspace so `cube workspace list`
    /// can surface it without re-running `jj status` on every workspace.
    /// Only updates free workspaces (leased workspaces have no meaningful
    /// health status until they are released).
    pub fn update_workspace_health(
        &self,
        repo: &str,
        workspace_id: &str,
        health: WorkspaceHealth,
    ) -> Result<(), CubeError> {
        self.connection
            .execute(
                r#"
                UPDATE workspaces
                SET health_status = ?3
                WHERE repo = ?1 AND workspace_id = ?2 AND state = ?4
                "#,
                params![
                    repo,
                    workspace_id,
                    health.as_str(),
                    WorkspaceState::Free.as_str(),
                ],
            )
            .map_err(CubeError::Storage)?;
        Ok(())
    }

    /// Claim a specific workspace by `workspace_id`, atomically transitioning
    /// it from `free` to `leased`. Returns `None` if the workspace is not
    /// currently free (e.g., already leased or doesn't exist). Unlike
    /// `claim_workspace`, there is no fallback — the named workspace must be
    /// free or the call returns `None`.
    pub fn claim_specific_workspace(
        &mut self,
        repo: &str,
        workspace_id: &str,
        holder: &str,
        task: &str,
        lease_id: &str,
        leased_at_epoch_s: i64,
        lease_expires_at_epoch_s: Option<i64>,
    ) -> Result<Option<WorkspaceRecord>, CubeError> {
        let transaction = self.connection.transaction().map_err(CubeError::Storage)?;

        let rows_updated = transaction
            .execute(
                r#"
                UPDATE workspaces
                SET
                    state = ?1,
                    lease_id = ?2,
                    holder = ?3,
                    task = ?4,
                    leased_at_epoch_s = ?5,
                    lease_expires_at_epoch_s = ?6,
                    head_commit = NULL,
                    last_release_reason = NULL,
                    health_status = NULL
                WHERE repo = ?7 AND workspace_id = ?8 AND state = ?9
                "#,
                params![
                    WorkspaceState::Leased.as_str(),
                    lease_id,
                    holder,
                    task,
                    leased_at_epoch_s,
                    lease_expires_at_epoch_s,
                    repo,
                    workspace_id,
                    WorkspaceState::Free.as_str(),
                ],
            )
            .map_err(CubeError::Storage)?;

        if rows_updated == 0 {
            transaction.rollback().map_err(CubeError::Storage)?;
            return Ok(None);
        }

        let claimed = transaction
            .query_row(
                r#"
                SELECT
                    repo,
                    workspace_id,
                    workspace_path,
                    state,
                    lease_id,
                    holder,
                    task,
                    leased_at_epoch_s,
                    lease_expires_at_epoch_s,
                    head_commit,
                    last_release_reason,
                    health_status
                FROM workspaces
                WHERE repo = ?1 AND workspace_id = ?2
                "#,
                params![repo, workspace_id],
                row_to_workspace_record,
            )
            .map_err(CubeError::Storage)?;
        transaction.commit().map_err(CubeError::Storage)?;
        Ok(Some(claimed))
    }

    pub fn get_workspace_by_path(
        &self,
        workspace_path: &Path,
    ) -> Result<Option<WorkspaceRecord>, CubeError> {
        self.connection
            .query_row(
                r#"
                SELECT
                    repo,
                    workspace_id,
                    workspace_path,
                    state,
                    lease_id,
                    holder,
                    task,
                    leased_at_epoch_s,
                    lease_expires_at_epoch_s,
                    head_commit,
                    last_release_reason,
                    health_status
                FROM workspaces
                WHERE workspace_path = ?1
                "#,
                params![workspace_path.display().to_string()],
                row_to_workspace_record,
            )
            .optional()
            .map_err(CubeError::Storage)
    }

    pub fn get_workspace_by_lease(
        &self,
        lease_id: &str,
    ) -> Result<Option<WorkspaceRecord>, CubeError> {
        self.connection
            .query_row(
                r#"
                SELECT
                    repo,
                    workspace_id,
                    workspace_path,
                    state,
                    lease_id,
                    holder,
                    task,
                    leased_at_epoch_s,
                    lease_expires_at_epoch_s,
                    head_commit,
                    last_release_reason,
                    health_status
                FROM workspaces
                WHERE lease_id = ?1
                "#,
                params![lease_id],
                row_to_workspace_record,
            )
            .optional()
            .map_err(CubeError::Storage)
    }

    pub fn forget_workspace(&self, repo: &str, workspace_id: &str) -> Result<(), CubeError> {
        self.connection
            .execute(
                r#"
                DELETE FROM workspaces
                WHERE repo = ?1 AND workspace_id = ?2
                "#,
                params![repo, workspace_id],
            )
            .map_err(CubeError::Storage)?;
        Ok(())
    }

    pub fn release_workspace(
        &self,
        lease_id: &str,
        reason: Option<&str>,
    ) -> Result<Option<WorkspaceRecord>, CubeError> {
        let before = self.get_workspace_by_lease(lease_id)?;
        let Some(record) = before else {
            return Ok(None);
        };

        self.connection
            .execute(
                r#"
                UPDATE workspaces
                SET
                    state = ?2,
                    lease_id = NULL,
                    holder = NULL,
                    task = NULL,
                    leased_at_epoch_s = NULL,
                    lease_expires_at_epoch_s = NULL,
                    head_commit = NULL,
                    last_release_reason = ?3,
                    health_status = NULL
                WHERE lease_id = ?1
                "#,
                params![lease_id, WorkspaceState::Free.as_str(), reason],
            )
            .map_err(CubeError::Storage)?;

        Ok(Some(WorkspaceRecord {
            state: WorkspaceState::Free,
            lease_id: None,
            holder: None,
            task: None,
            leased_at_epoch_s: None,
            lease_expires_at_epoch_s: None,
            head_commit: None,
            last_release_reason: reason.map(str::to_string),
            health_status: None,
            ..record
        }))
    }

    pub fn get_workspace_setup_state(
        &self,
        repo: &str,
        workspace_id: &str,
        step_id: &str,
    ) -> Result<Option<WorkspaceSetupState>, CubeError> {
        self.connection
            .query_row(
                r#"
                SELECT repo, workspace_id, step_id, fingerprint, last_run_epoch_s
                FROM workspace_setup
                WHERE repo = ?1 AND workspace_id = ?2 AND step_id = ?3
                "#,
                params![repo, workspace_id, step_id],
                row_to_workspace_setup_state,
            )
            .optional()
            .map_err(CubeError::Storage)
    }

    pub fn get_pool_metadata_i(&self, key: &str) -> Result<Option<i64>, CubeError> {
        self.connection
            .query_row(
                "SELECT value_i FROM pool_metadata WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()
            .map_err(CubeError::Storage)
    }

    pub fn set_pool_metadata_i(&self, key: &str, value: i64) -> Result<(), CubeError> {
        self.connection
            .execute(
                "INSERT INTO pool_metadata (key, value_i) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value_i = excluded.value_i",
                params![key, value],
            )
            .map_err(CubeError::Storage)?;
        Ok(())
    }

    pub fn upsert_workspace_setup_state(
        &self,
        state: &WorkspaceSetupState,
    ) -> Result<(), CubeError> {
        self.connection
            .execute(
                r#"
                INSERT INTO workspace_setup (
                    repo,
                    workspace_id,
                    step_id,
                    fingerprint,
                    last_run_epoch_s
                ) VALUES (?1, ?2, ?3, ?4, ?5)
                ON CONFLICT(repo, workspace_id, step_id) DO UPDATE SET
                    fingerprint = excluded.fingerprint,
                    last_run_epoch_s = excluded.last_run_epoch_s
                "#,
                params![
                    state.repo,
                    state.workspace_id,
                    state.step_id,
                    state.fingerprint,
                    state.last_run_epoch_s,
                ],
            )
            .map_err(CubeError::Storage)?;
        Ok(())
    }

    pub fn list_workspace_setup_states(
        &self,
        repo: &str,
        workspace_id: &str,
    ) -> Result<Vec<WorkspaceSetupState>, CubeError> {
        let mut statement = self
            .connection
            .prepare(
                r#"
                SELECT repo, workspace_id, step_id, fingerprint, last_run_epoch_s
                FROM workspace_setup
                WHERE repo = ?1 AND workspace_id = ?2
                ORDER BY step_id
                "#,
            )
            .map_err(CubeError::Storage)?;
        let rows = statement
            .query_map(params![repo, workspace_id], row_to_workspace_setup_state)
            .map_err(CubeError::Storage)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(CubeError::Storage)
    }

    /// Force-release a lease without checking holder ownership. Used for
    /// orphan reclamation on engine restart.
    pub fn force_release_lease(
        &self,
        lease_id: &str,
        reason: Option<&str>,
    ) -> Result<Option<WorkspaceRecord>, CubeError> {
        // Same SQL as release_workspace today; the distinction is at the
        // CLI/callsite level where force-release skips the workspace reset.
        // Holder/ownership checks aren't enforced anywhere in the store yet,
        // so this is a thin wrapper for now.
        self.release_workspace(lease_id, reason)
    }

    /// Update the expiry for a leased workspace. Returns the updated row,
    /// or None if no workspace currently holds the lease id.
    pub fn heartbeat_lease(
        &self,
        lease_id: &str,
        new_expires_at_epoch_s: Option<i64>,
    ) -> Result<Option<WorkspaceRecord>, CubeError> {
        let updated = self
            .connection
            .execute(
                r#"
                UPDATE workspaces
                SET lease_expires_at_epoch_s = ?2
                WHERE lease_id = ?1 AND state = ?3
                "#,
                params![
                    lease_id,
                    new_expires_at_epoch_s,
                    WorkspaceState::Leased.as_str(),
                ],
            )
            .map_err(CubeError::Storage)?;
        if updated == 0 {
            return Ok(None);
        }
        self.get_workspace_by_lease(lease_id)
    }

    /// Sweep leases whose expiry is at or before `now_epoch_s`; flip them
    /// back to `free` and record `expired` as the release reason. Returns
    /// one [`ExpiredLease`] entry per swept row so callers can audit them
    /// and apply per-workspace policy (e.g., refuse to destructively
    /// reset a workspace whose `@` still has the prior lease holder's
    /// work) — the original signature only returned a count, which made
    /// it impossible for the lease handler to tell apart "claimed a
    /// genuinely free workspace" from "claimed a workspace that was
    /// just reclaimed out from under a still-active worker."
    pub fn expire_stale_leases(
        &self,
        repo: &str,
        now_epoch_s: i64,
    ) -> Result<Vec<ExpiredLease>, CubeError> {
        let transaction = self.connection.unchecked_transaction().map_err(CubeError::Storage)?;
        let swept = {
            let mut statement = transaction
                .prepare(
                    r#"
                    SELECT workspace_id, lease_id, holder, task, leased_at_epoch_s, lease_expires_at_epoch_s
                    FROM workspaces
                    WHERE repo = ?1
                      AND state = ?2
                      AND lease_expires_at_epoch_s IS NOT NULL
                      AND lease_expires_at_epoch_s <= ?3
                    "#,
                )
                .map_err(CubeError::Storage)?;
            let rows = statement
                .query_map(
                    params![repo, WorkspaceState::Leased.as_str(), now_epoch_s],
                    |row| {
                        Ok(ExpiredLease {
                            workspace_id: row.get(0)?,
                            lease_id: row.get(1)?,
                            holder: row.get(2)?,
                            task: row.get(3)?,
                            leased_at_epoch_s: row.get(4)?,
                            lease_expires_at_epoch_s: row.get(5)?,
                        })
                    },
                )
                .map_err(CubeError::Storage)?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(CubeError::Storage)?
        };
        if swept.is_empty() {
            transaction.rollback().map_err(CubeError::Storage)?;
            return Ok(swept);
        }
        transaction
            .execute(
                r#"
                UPDATE workspaces
                SET
                    state = ?2,
                    lease_id = NULL,
                    holder = NULL,
                    task = NULL,
                    leased_at_epoch_s = NULL,
                    lease_expires_at_epoch_s = NULL,
                    head_commit = NULL,
                    last_release_reason = 'expired'
                WHERE repo = ?1
                  AND state = ?3
                  AND lease_expires_at_epoch_s IS NOT NULL
                  AND lease_expires_at_epoch_s <= ?4
                "#,
                params![
                    repo,
                    WorkspaceState::Free.as_str(),
                    WorkspaceState::Leased.as_str(),
                    now_epoch_s,
                ],
            )
            .map_err(CubeError::Storage)?;
        transaction.commit().map_err(CubeError::Storage)?;
        Ok(swept)
    }

    pub fn insert_change(&self, record: &ChangeRecord) -> Result<ChangeRecord, CubeError> {
        self.connection
            .execute(
                r#"
                INSERT INTO changes (
                    change_id,
                    repo,
                    workspace_path,
                    parent_change_id,
                    title,
                    jj_change_id,
                    head_commit,
                    created_at_epoch_s
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                "#,
                params![
                    record.change_id,
                    record.repo,
                    path_to_string(&record.workspace_path),
                    record.parent_change_id,
                    record.title,
                    record.jj_change_id,
                    record.head_commit,
                    record.created_at_epoch_s,
                ],
            )
            .map_err(CubeError::Storage)?;

        self.get_change(&record.change_id)?
            .ok_or_else(|| CubeError::ChangeNotFound(record.change_id.clone()))
    }

    pub fn get_change(&self, change_id: &str) -> Result<Option<ChangeRecord>, CubeError> {
        self.connection
            .query_row(
                r#"
                SELECT
                    change_id,
                    repo,
                    workspace_path,
                    parent_change_id,
                    title,
                    jj_change_id,
                    head_commit,
                    created_at_epoch_s
                FROM changes
                WHERE change_id = ?1
                "#,
                params![change_id],
                row_to_change_record,
            )
            .optional()
            .map_err(CubeError::Storage)
    }

    fn migrate(&self) -> Result<(), CubeError> {
        self.connection
            .execute_batch(
                r#"
                PRAGMA foreign_keys = ON;

                CREATE TABLE IF NOT EXISTS repos (
                    repo TEXT PRIMARY KEY,
                    origin TEXT NOT NULL,
                    main_branch TEXT NOT NULL,
                    workspace_root TEXT NOT NULL,
                    workspace_prefix TEXT NOT NULL,
                    source_path TEXT
                );

                CREATE INDEX IF NOT EXISTS repos_origin_idx
                    ON repos(origin);

                CREATE TABLE IF NOT EXISTS workspaces (
                    repo TEXT NOT NULL,
                    workspace_id TEXT NOT NULL,
                    workspace_path TEXT NOT NULL UNIQUE,
                    state TEXT NOT NULL,
                    lease_id TEXT,
                    holder TEXT,
                    task TEXT,
                    leased_at_epoch_s INTEGER,
                    lease_expires_at_epoch_s INTEGER,
                    head_commit TEXT,
                    last_release_reason TEXT,
                    health_status TEXT,
                    PRIMARY KEY(repo, workspace_id),
                    FOREIGN KEY(repo) REFERENCES repos(repo) ON DELETE CASCADE
                );

                CREATE INDEX IF NOT EXISTS workspaces_repo_state_idx
                    ON workspaces(repo, state);

                CREATE TABLE IF NOT EXISTS changes (
                    change_id TEXT PRIMARY KEY,
                    repo TEXT NOT NULL,
                    workspace_path TEXT NOT NULL,
                    parent_change_id TEXT,
                    title TEXT NOT NULL,
                    jj_change_id TEXT NOT NULL,
                    head_commit TEXT NOT NULL,
                    created_at_epoch_s INTEGER NOT NULL,
                    FOREIGN KEY(repo) REFERENCES repos(repo) ON DELETE CASCADE,
                    FOREIGN KEY(parent_change_id) REFERENCES changes(change_id) ON DELETE SET NULL
                );

                CREATE INDEX IF NOT EXISTS changes_repo_created_idx
                    ON changes(repo, created_at_epoch_s);

                CREATE TABLE IF NOT EXISTS workspace_setup (
                    repo TEXT NOT NULL,
                    workspace_id TEXT NOT NULL,
                    step_id TEXT NOT NULL,
                    fingerprint TEXT NOT NULL,
                    last_run_epoch_s INTEGER NOT NULL,
                    PRIMARY KEY(repo, workspace_id, step_id),
                    FOREIGN KEY(repo, workspace_id)
                        REFERENCES workspaces(repo, workspace_id)
                        ON DELETE CASCADE
                );

                CREATE TABLE IF NOT EXISTS pool_metadata (
                    key TEXT PRIMARY KEY,
                    value_i INTEGER
                );
                "#,
            )
            .map_err(CubeError::Storage)?;

        // Additive column upgrades for existing databases. New CREATE TABLE
        // already includes these, but ALTER lets us pick them up on a DB
        // that pre-dates this code.
        try_add_column(
            &self.connection,
            "ALTER TABLE workspaces ADD COLUMN lease_expires_at_epoch_s INTEGER",
        )?;
        try_add_column(
            &self.connection,
            "ALTER TABLE workspaces ADD COLUMN last_release_reason TEXT",
        )?;
        try_add_column(
            &self.connection,
            "ALTER TABLE workspaces ADD COLUMN health_status TEXT",
        )?;

        Ok(())
    }
}

fn try_add_column(connection: &Connection, sql: &str) -> Result<(), CubeError> {
    match connection.execute(sql, []) {
        Ok(_) => Ok(()),
        Err(rusqlite::Error::SqliteFailure(_, Some(msg)))
            if msg.contains("duplicate column name") =>
        {
            Ok(())
        }
        Err(e) => Err(CubeError::Storage(e)),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceSetupState {
    pub repo: String,
    pub workspace_id: String,
    pub step_id: String,
    pub fingerprint: String,
    pub last_run_epoch_s: i64,
}

fn row_to_workspace_setup_state(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<WorkspaceSetupState> {
    Ok(WorkspaceSetupState {
        repo: row.get(0)?,
        workspace_id: row.get(1)?,
        step_id: row.get(2)?,
        fingerprint: row.get(3)?,
        last_run_epoch_s: row.get(4)?,
    })
}

fn path_to_string(path: &Path) -> String {
    path.display().to_string()
}

fn row_to_repo_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<RepoRecord> {
    Ok(RepoRecord {
        repo: row.get(0)?,
        origin: row.get(1)?,
        main_branch: row.get(2)?,
        workspace_root: row.get::<_, String>(3)?.into(),
        workspace_prefix: row.get(4)?,
        source: row.get::<_, Option<String>>(5)?.map(Into::into),
    })
}

fn row_to_workspace_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<WorkspaceRecord> {
    let state_raw: String = row.get(3)?;
    let health_raw: Option<String> = row.get(11)?;
    Ok(WorkspaceRecord {
        repo: row.get(0)?,
        workspace_id: row.get(1)?,
        workspace_path: row.get::<_, String>(2)?.into(),
        state: WorkspaceState::from_str(&state_raw).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                3,
                rusqlite::types::Type::Text,
                Box::<dyn std::error::Error + Send + Sync>::from(format!(
                    "invalid workspace state `{state_raw}`"
                )),
            )
        })?,
        lease_id: row.get(4)?,
        holder: row.get(5)?,
        task: row.get(6)?,
        leased_at_epoch_s: row.get(7)?,
        lease_expires_at_epoch_s: row.get(8)?,
        head_commit: row.get(9)?,
        last_release_reason: row.get(10)?,
        health_status: health_raw.as_deref().and_then(WorkspaceHealth::from_str),
    })
}

fn row_to_change_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<ChangeRecord> {
    Ok(ChangeRecord {
        change_id: row.get(0)?,
        repo: row.get(1)?,
        workspace_path: row.get::<_, String>(2)?.into(),
        parent_change_id: row.get(3)?,
        title: row.get(4)?,
        jj_change_id: row.get(5)?,
        head_commit: row.get(6)?,
        created_at_epoch_s: row.get(7)?,
    })
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use crate::metadata::{
        ChangeRecord, RepoRecord, WorkspaceCandidate, WorkspaceRecord, WorkspaceState,
    };

    use super::{EffectiveState, Store, WorkspaceListFilter};

    fn open_store() -> (TempDir, Store) {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let store = Store::open_at(tempdir.path().join("state.db")).expect("store");
        (tempdir, store)
    }

    #[test]
    fn list_workspaces_defaults_to_empty() {
        let (_tempdir, store) = open_store();
        let config = RepoRecord {
            repo: "mono".to_string(),
            origin: "git@github.com:spinyfin/mono.git".to_string(),
            main_branch: "main".to_string(),
            workspace_root: "/tmp/workspaces".into(),
            workspace_prefix: "mono-agent-".to_string(),
            source: None,
        };
        store.upsert_repo(&config).expect("repo");

        let workspaces = store.list_workspaces("mono").expect("workspaces");
        assert!(workspaces.is_empty());
    }

    #[test]
    fn sync_workspaces_prunes_deleted_free_rows() {
        let (tempdir, mut store) = open_store();
        let workspace_root = tempdir.path().join("workspaces");
        let config = RepoRecord {
            repo: "mono".to_string(),
            origin: "git@github.com:spinyfin/mono.git".to_string(),
            main_branch: "main".to_string(),
            workspace_root: workspace_root.clone(),
            workspace_prefix: "mono-agent-".to_string(),
            source: None,
        };
        store.upsert_repo(&config).expect("repo");

        store
            .sync_workspaces(
                "mono",
                &[
                    WorkspaceCandidate {
                        workspace_id: "mono-agent-001".to_string(),
                        workspace_path: workspace_root.join("mono-agent-001"),
                    },
                    WorkspaceCandidate {
                        workspace_id: "mono-agent-002".to_string(),
                        workspace_path: workspace_root.join("mono-agent-002"),
                    },
                ],
            )
            .expect("initial sync");

        store
            .sync_workspaces(
                "mono",
                &[WorkspaceCandidate {
                    workspace_id: "mono-agent-002".to_string(),
                    workspace_path: workspace_root.join("mono-agent-002"),
                }],
            )
            .expect("prune sync");

        let workspaces = store.list_workspaces("mono").expect("workspaces");
        assert_eq!(workspaces.len(), 1);
        assert_eq!(workspaces[0].workspace_id, "mono-agent-002");
    }

    #[test]
    fn list_workspaces_filtered_applies_repo_state_and_holder() {
        let (tempdir, mut store) = open_store();
        let workspace_root = tempdir.path().join("workspaces");
        for repo in ["mono", "flunge"] {
            store
                .upsert_repo(&RepoRecord {
                    repo: repo.to_string(),
                    origin: format!("git@example.com:org/{repo}.git"),
                    main_branch: "main".to_string(),
                    workspace_root: workspace_root.clone(),
                    workspace_prefix: format!("{repo}-agent-"),
                    source: None,
                })
                .expect("repo");
            store
                .sync_workspaces(
                    repo,
                    &[
                        WorkspaceCandidate {
                            workspace_id: format!("{repo}-agent-001"),
                            workspace_path: workspace_root.join(format!("{repo}-agent-001")),
                        },
                        WorkspaceCandidate {
                            workspace_id: format!("{repo}-agent-002"),
                            workspace_path: workspace_root.join(format!("{repo}-agent-002")),
                        },
                    ],
                )
                .expect("sync");
        }

        // lease one workspace in each repo with distinct holders
        store
            .claim_workspace(
                "mono",
                "boss/worker-7",
                "demo",
                "lease-mono",
                100,
                Some(1900),
                None,
            )
            .expect("claim mono");
        store
            .claim_workspace(
                "flunge",
                "alice@host:42",
                "fix",
                "lease-flunge",
                100,
                Some(1900),
                None,
            )
            .expect("claim flunge");

        // unfiltered: 4 workspaces total, ordered by repo then id
        let all = store
            .list_workspaces_filtered(&WorkspaceListFilter::default())
            .expect("list");
        assert_eq!(all.len(), 4);
        assert_eq!(
            all.iter()
                .map(|r| format!("{}/{}", r.repo, r.workspace_id))
                .collect::<Vec<_>>(),
            vec![
                "flunge/flunge-agent-001",
                "flunge/flunge-agent-002",
                "mono/mono-agent-001",
                "mono/mono-agent-002",
            ]
        );

        // repo filter
        let mono = store
            .list_workspaces_filtered(&WorkspaceListFilter {
                repo: Some("mono"),
                ..Default::default()
            })
            .expect("list mono");
        assert_eq!(mono.len(), 2);
        assert!(mono.iter().all(|r| r.repo == "mono"));

        // state filter
        let leased = store
            .list_workspaces_filtered(&WorkspaceListFilter {
                effective_state: Some(EffectiveState::Leased),
                ..Default::default()
            })
            .expect("list leased");
        assert_eq!(leased.len(), 2);
        assert!(leased.iter().all(|r| r.state == WorkspaceState::Leased));

        // holder GLOB filter
        let boss_owned = store
            .list_workspaces_filtered(&WorkspaceListFilter {
                holder_pattern: Some("boss/*"),
                ..Default::default()
            })
            .expect("list boss");
        assert_eq!(boss_owned.len(), 1);
        assert_eq!(boss_owned[0].holder.as_deref(), Some("boss/worker-7"));

        // combined filters
        let mono_free = store
            .list_workspaces_filtered(&WorkspaceListFilter {
                repo: Some("mono"),
                effective_state: Some(EffectiveState::Free),
                ..Default::default()
            })
            .expect("list mono free");
        assert_eq!(mono_free.len(), 1);
        assert_eq!(mono_free[0].workspace_id, "mono-agent-002");
    }

    #[test]
    fn ttl_lifecycle_expires_and_heartbeat_extends() {
        let (tempdir, mut store) = open_store();
        let workspace_root = tempdir.path().join("workspaces");
        store
            .upsert_repo(&RepoRecord {
                repo: "mono".to_string(),
                origin: "git@example.com:org/mono.git".to_string(),
                main_branch: "main".to_string(),
                workspace_root: workspace_root.clone(),
                workspace_prefix: "mono-agent-".to_string(),
                source: None,
            })
            .expect("repo");
        store
            .sync_workspaces(
                "mono",
                &[
                    WorkspaceCandidate {
                        workspace_id: "mono-agent-001".to_string(),
                        workspace_path: workspace_root.join("mono-agent-001"),
                    },
                    WorkspaceCandidate {
                        workspace_id: "mono-agent-002".to_string(),
                        workspace_path: workspace_root.join("mono-agent-002"),
                    },
                ],
            )
            .expect("sync");

        // Lease two workspaces with TTL of 100s, leased at t=0
        let lease_a = store
            .claim_workspace("mono", "alice", "fix", "lease-a", 0, Some(100), None)
            .expect("claim a")
            .expect("got record");
        let lease_b = store
            .claim_workspace("mono", "bob", "review", "lease-b", 0, Some(100), None)
            .expect("claim b")
            .expect("got record");
        assert_eq!(lease_a.lease_expires_at_epoch_s, Some(100));
        assert_eq!(lease_b.lease_expires_at_epoch_s, Some(100));

        // Heartbeat lease-a to extend it to t=500
        let beat = store
            .heartbeat_lease("lease-a", Some(500))
            .expect("heartbeat")
            .expect("found");
        assert_eq!(beat.lease_expires_at_epoch_s, Some(500));

        // Sweep at t=200 — only lease-b is past its TTL
        let reclaimed = store.expire_stale_leases("mono", 200).expect("sweep");
        assert_eq!(reclaimed.len(), 1);
        let swept = &reclaimed[0];
        assert_eq!(swept.workspace_id, "mono-agent-002");
        assert_eq!(swept.lease_id, "lease-b");
        assert_eq!(swept.holder.as_deref(), Some("bob"));
        assert_eq!(swept.task.as_deref(), Some("review"));
        assert_eq!(swept.lease_expires_at_epoch_s, Some(100));

        let after = store
            .list_workspaces_filtered(&WorkspaceListFilter {
                repo: Some("mono"),
                ..Default::default()
            })
            .expect("list");
        let by_id: std::collections::HashMap<&str, &WorkspaceRecord> = after
            .iter()
            .map(|r| (r.workspace_id.as_str(), r))
            .collect();

        let a = by_id["mono-agent-001"];
        assert_eq!(a.state, WorkspaceState::Leased);
        assert_eq!(a.lease_id.as_deref(), Some("lease-a"));

        let b = by_id["mono-agent-002"];
        assert_eq!(b.state, WorkspaceState::Free);
        assert!(b.lease_id.is_none());
        assert_eq!(b.last_release_reason.as_deref(), Some("expired"));
    }

    #[test]
    fn release_records_reason() {
        let (tempdir, mut store) = open_store();
        let workspace_root = tempdir.path().join("workspaces");
        store
            .upsert_repo(&RepoRecord {
                repo: "mono".to_string(),
                origin: "git@example.com:org/mono.git".to_string(),
                main_branch: "main".to_string(),
                workspace_root: workspace_root.clone(),
                workspace_prefix: "mono-agent-".to_string(),
                source: None,
            })
            .expect("repo");
        store
            .sync_workspaces(
                "mono",
                &[WorkspaceCandidate {
                    workspace_id: "mono-agent-001".to_string(),
                    workspace_path: workspace_root.join("mono-agent-001"),
                }],
            )
            .expect("sync");
        store
            .claim_workspace("mono", "alice", "fix", "lease-a", 0, Some(100), None)
            .expect("claim");

        let released = store
            .release_workspace("lease-a", Some("crash"))
            .expect("release")
            .expect("found");
        assert_eq!(released.last_release_reason.as_deref(), Some("crash"));
        assert_eq!(released.state, WorkspaceState::Free);
        assert!(released.lease_expires_at_epoch_s.is_none());
    }

    #[test]
    fn insert_and_get_change_round_trip() {
        let (tempdir, store) = open_store();
        let workspace_root = tempdir.path().join("workspaces");
        let config = RepoRecord {
            repo: "mono".to_string(),
            origin: "git@github.com:spinyfin/mono.git".to_string(),
            main_branch: "main".to_string(),
            workspace_root,
            workspace_prefix: "mono-agent-".to_string(),
            source: None,
        };
        store.upsert_repo(&config).expect("repo");

        let change = ChangeRecord {
            change_id: "chg_test".to_string(),
            repo: "mono".to_string(),
            workspace_path: tempdir.path().join("workspaces/mono-agent-001"),
            parent_change_id: None,
            title: "Add parser".to_string(),
            jj_change_id: "abc123xyz".to_string(),
            head_commit: "d34db33".to_string(),
            created_at_epoch_s: 123,
        };

        let inserted = store.insert_change(&change).expect("change");
        assert_eq!(inserted, change);
        let fetched = store.get_change("chg_test").expect("get");
        assert_eq!(fetched, Some(change));
    }
}
