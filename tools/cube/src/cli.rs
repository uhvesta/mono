use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "cube")]
#[command(about = "Manage reusable agent workspaces and stacked changes")]
pub struct Cli {
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    Repo {
        #[command(subcommand)]
        command: RepoCommand,
    },
    Workspace {
        #[command(subcommand)]
        command: WorkspaceCommand,
    },
    Change {
        #[command(subcommand)]
        command: ChangeCommand,
    },
    Stack {
        #[command(subcommand)]
        command: StackCommand,
    },
    Pr {
        #[command(subcommand)]
        command: PrCommand,
    },
    Graph(GraphArgs),
    Doctor(DoctorArgs),
}

#[derive(Debug, Subcommand)]
pub enum RepoCommand {
    /// Resolve or materialize a repo pool from a name or origin URL.
    ///
    /// Pass a bare `<reponame>` to resolve it through the chain:
    /// registered slug → configured `repo-resolvers` → GitHub
    /// `<org>/<name>` fallback. Or pass `--origin <git-url>` to skip
    /// resolution and use the URL directly.
    Ensure {
        /// Repo name to resolve via the resolver chain.
        #[arg(conflicts_with = "origin", required_unless_present = "origin")]
        reponame: Option<String>,
        /// Origin URL for the repo (bypasses name resolution).
        #[arg(long, conflicts_with = "reponame")]
        origin: Option<String>,
    },
    /// List known repo pools.
    List,
    /// Show repo pool configuration.
    Info {
        /// Stable repo identifier such as `mono`.
        repo: String,
    },
    /// Remove a repo pool and all its workspaces and changes.
    ///
    /// Deletes the repo row and cascades to workspace, workspace_setup, and
    /// change records. By default the on-disk workspace directories are left
    /// intact; pass `--purge-workspaces` to delete them too.
    Remove {
        /// Stable repo identifier such as `mono`.
        repo: String,
        /// Remove even if one or more workspaces in the pool are currently leased.
        #[arg(long)]
        force: bool,
        /// Also delete on-disk workspace directories under the pool's workspace_root.
        #[arg(long)]
        purge_workspaces: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum WorkspaceCommand {
    /// Lease a reusable workspace for one task.
    Lease {
        /// Repo identifier to lease from.
        repo: String,
        /// Short task summary recorded with the lease.
        #[arg(long)]
        task: String,
        /// Preferred workspace id to lease (best-effort). If the named
        /// workspace is free it will be leased; otherwise lease falls
        /// back to the first free workspace in the repo.
        #[arg(long)]
        prefer: Option<String>,
        /// Reclaim the `--prefer` workspace *with its dirty working copy
        /// intact*, for recovery of stranded in-flight work. The normal
        /// lease path skips a dirty workspace and provisions a fresh one;
        /// with this flag cube claims the named workspace as-is and
        /// suppresses the `jj git fetch && jj new main` reset so the
        /// uncommitted tree is handed to the new lease-holder. Requires
        /// `--prefer`. Unlike the best-effort `--prefer`, this never
        /// falls back: if the named workspace is missing, leased, or has
        /// no repo, the lease fails loudly rather than routing the
        /// recovering worker away from the only copy of the work.
        /// Mutually exclusive with `--resume-pr`.
        #[arg(long, requires = "prefer", conflicts_with = "resume_pr")]
        allow_dirty: bool,
        /// Resume an existing GitHub PR by number. After the workspace is
        /// claimed, replaces the normal `jj new <main>` reset with a PR
        /// positioning sequence: fetch from the GitHub remote, resolve
        /// PR N's current head, reconcile the local `pr/<n>` and head-
        /// branch bookmarks, then `jj new pr/<n>`. The working copy
        /// lands as a fresh empty commit ready to edit on top of PR N's
        /// head. Composes with `--prefer` (warm workspace uses the local
        /// `pr/<n>` bookmark; cold workspace falls back to `gh pr view`).
        /// Mutually exclusive with `--allow-dirty`.
        #[arg(long, conflicts_with = "allow_dirty")]
        resume_pr: Option<u64>,
    },
    /// Release a workspace lease.
    ///
    /// Pass either a workspace id positionally (e.g.
    /// `cube workspace release mono-agent-004`) or the lease uuid
    /// via `--lease`. Use `--repo` to disambiguate when the same
    /// workspace id exists under multiple repos.
    Release {
        /// Workspace id to release (e.g. `mono-agent-004`).
        #[arg(conflicts_with = "lease", required_unless_present = "lease")]
        workspace: Option<String>,
        /// Lease id returned by `workspace lease`.
        #[arg(long, conflicts_with = "workspace")]
        lease: Option<String>,
        /// Optional repo filter; only used with the workspace-id form.
        #[arg(long, requires = "workspace")]
        repo: Option<String>,
        /// Annotate the release with a reason (e.g. `crash`, `oom`).
        /// Recorded in the workspaces row's `last_release_reason`.
        #[arg(long)]
        reason: Option<String>,
        /// Skip the `jj git fetch && jj new main` reset on release.
        /// The slot is freed in cube's registry but the workspace's
        /// working copy is left as-is for forensics. Pair with
        /// `--reason crash` for crash recovery.
        #[arg(long)]
        keep_dirty: bool,
    },
    /// Refresh a lease's expiry so it isn't reclaimed by the TTL sweep.
    ///
    /// Boss-engine pings this on a timer to keep its leases alive.
    Heartbeat {
        /// Lease id to refresh.
        #[arg(long)]
        lease: String,
        /// Override the new TTL in seconds. Defaults to the standard
        /// 1800s window from now.
        #[arg(long)]
        ttl_seconds: Option<u64>,
    },
    /// Force-release a lease without running the workspace reset.
    ///
    /// Bypasses ownership / holder checks. Intended for orphan
    /// reclamation after a holder process has crashed; pair with
    /// `cube workspace list --holder <pattern>` to find candidates.
    ForceRelease {
        /// Workspace id to release.
        #[arg(conflicts_with = "lease", required_unless_present = "lease")]
        workspace: Option<String>,
        /// Lease id to release.
        #[arg(long, conflicts_with = "workspace")]
        lease: Option<String>,
        /// Optional repo filter; only used with the workspace-id form.
        #[arg(long, requires = "workspace")]
        repo: Option<String>,
        /// Annotate the release with a reason (defaults to `force-released`).
        #[arg(long)]
        reason: Option<String>,
    },
    /// Inspect workspace lease state.
    Status {
        /// Absolute workspace path.
        #[arg(long)]
        workspace: String,
    },
    /// Run workspace setup steps when configured.
    Setup {
        /// Absolute workspace path.
        #[arg(long)]
        workspace: String,
    },
    /// List workspaces in the registry.
    ///
    /// See also `cube workspace lease`, `release`, `force-release`,
    /// `heartbeat`, `status`, `setup`, `remove`.
    List {
        /// Filter by repo id.
        #[arg(long)]
        repo: Option<String>,
        /// Filter by state (`free` or `leased`).
        #[arg(long)]
        state: Option<String>,
        /// Filter by holder. SQLite GLOB pattern — `*` matches anything,
        /// e.g. `--holder boss/*`.
        #[arg(long)]
        holder: Option<String>,
    },
    /// Forget consumed boss/exec_* bookmarks from workspace pools.
    ///
    /// A bookmark is "consumed" when its tip is reachable from `main`
    /// (i.e. its PR has merged). Without `--workspace`, iterates every
    /// workspace in the pool; leased workspaces are skipped.
    Gc {
        /// Only process this workspace id.
        #[arg(long)]
        workspace: Option<String>,
        /// List what would be forgotten without doing it.
        #[arg(long)]
        dry_run: bool,
    },
    /// Rebase the current workspace's boss branch onto the repo's integration branch.
    ///
    /// Fetches the latest integration branch (e.g. `main`, `master`, `trunk`)
    /// and boss branches from the GitHub remote, resolves this workspace's
    /// current `boss/exec_*` branch automatically from the working copy state
    /// (no branch name argument required), rebases it onto the configured
    /// integration branch using `--ignore-immutable` to handle jj's
    /// immutable-heads constraint, and reports the result clearly.
    ///
    /// The target branch is read from the repo pool configuration
    /// (`main_branch` field) — not hardcoded. Repos that use `master`,
    /// `trunk`, or any other name are handled automatically.
    ///
    /// Leaves any resulting conflicts materialized in the working copy for the
    /// agent to resolve. Exit signal:
    ///   - `REBASED_CLEAN`: rebase succeeded with no conflicts.
    ///   - `REBASED_WITH_CONFLICTS`: conflicts in working copy — resolve them.
    ///
    /// Run from inside the leased cube workspace directory.
    Rebase,
    /// Remove a workspace row from the registry.
    ///
    /// Deletes the `workspaces` row (and cascades `workspace_setup`)
    /// for the given workspace id. By default the on-disk workspace
    /// directory is left untouched — it may already be gone, or the
    /// operator may want to inspect it. Use this to clean up dangling
    /// registry rows after a workspace directory has been wiped
    /// manually.
    ///
    /// Pass `--expunge` to also `rm -rf` the workspace directory after
    /// the row is deleted. Without `--expunge`, the next lease against
    /// the same repo will rediscover the directory via
    /// `discover_workspaces` + `sync_workspaces` and resurrect the row
    /// as `state=Free`. The `--expunge` form makes the removal durable.
    ///
    /// Refuses leased rows unless `--force`. The safer surgical default
    /// is to `cube workspace force-release` first, then `remove`.
    Remove {
        /// Workspace id to remove (e.g. `mono-agent-004`).
        workspace: String,
        /// Optional repo filter; required only when the workspace id
        /// matches multiple repos.
        #[arg(long)]
        repo: Option<String>,
        /// Remove even if the row is currently leased.
        #[arg(long)]
        force: bool,
        /// Also delete the on-disk workspace directory after the row
        /// is removed. Without this flag the next lease will
        /// rediscover the directory and resurrect the row.
        #[arg(long)]
        expunge: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum ChangeCommand {
    /// Create a new change node.
    Create(ChangeCreateArgs),
    /// Move the working copy to a change.
    Checkout {
        /// Local cube change id.
        #[arg(long)]
        change: String,
    },
    /// Show local change metadata.
    Info {
        /// Local cube change id.
        #[arg(long)]
        change: String,
    },
}

#[derive(Debug, Args)]
pub struct ChangeCreateArgs {
    /// Absolute workspace path.
    #[arg(long)]
    pub workspace: Option<String>,
    /// Optional parent change id.
    #[arg(long)]
    pub parent: Option<String>,
    /// Change title.
    #[arg(long)]
    pub title: String,
}

#[derive(Debug, Subcommand)]
pub enum StackCommand {
    /// Rebase a stack root or subtree.
    Rebase(StackRebaseArgs),
}

#[derive(Debug, Args)]
pub struct StackRebaseArgs {
    /// Root change id for a linear stack rebase.
    #[arg(long, conflicts_with = "subtree")]
    pub root: Option<String>,
    /// Root change id for a subtree rebase.
    #[arg(long, conflicts_with = "root")]
    pub subtree: Option<String>,
    /// Target change id or integration branch.
    #[arg(long)]
    pub onto: String,
}

#[derive(Debug, Subcommand)]
pub enum PrCommand {
    /// Create or reuse a GitHub PR for the current jj bookmark.
    ///
    /// Resolves `owner/repo` from `jj git remote`, pushes the branch,
    /// checks for an existing open PR, and creates one if none exists.
    /// Prints the PR URL as the only stdout line. Idempotent — safe to
    /// re-run: if the PR already exists its URL is returned unchanged.
    /// Uses `-R <owner/repo>` with `gh` so no `GIT_DIR` guess is needed.
    Ensure(PrEnsureArgs),
    /// Advance an existing PR by pushing the current commit to its head branch.
    ///
    /// Advances both the remote head branch and the local `pr/<n>` bookmark to
    /// `@` (fast-forward only by default) and pushes to GitHub. Idempotent: a
    /// re-run with nothing new to land is a no-op. Refuses non-descendants,
    /// merged/closed PRs, and empty commits (unless already pushed).
    Push(PrPushArgs),
    /// Sync local change state to GitHub pull requests.
    Sync(PrSyncArgs),
    /// Merge one PR or a ready sub-stack.
    Merge(PrMergeArgs),
}

#[derive(Debug, Args)]
pub struct PrPushArgs {
    /// PR number to advance. If omitted, inferred from the nearest `pr/<n>`
    /// bookmark in `@`'s ancestry.
    #[arg(long)]
    pub pr: Option<u64>,
    /// Head branch name to push. If omitted, inferred from the co-located
    /// non-`pr/*` bookmark on the same commit as the `pr/<n>` ancestor.
    #[arg(long)]
    pub branch: Option<String>,
    /// Force-push with lease semantics: verifies that GitHub's head has not
    /// advanced beyond the last-fetched state before force-pushing. Required
    /// for rewrite scenarios (amend, rebase). The default push is
    /// fast-forward only.
    #[arg(long)]
    pub force_with_lease: bool,
}

#[derive(Debug, Args)]
pub struct PrEnsureArgs {
    /// Branch name to push and open a PR for.
    /// Defaults to the first bookmark on the current jj commit.
    #[arg(long)]
    pub branch: Option<String>,
    /// PR title (gh prompts interactively when omitted and stdin is a TTY).
    #[arg(long)]
    pub title: Option<String>,
    /// PR body text. WARNING: unsafe when the body contains backticks or $(...) because
    /// the shell evaluates them before cube sees the argument. Use --body-file instead.
    #[arg(long, conflicts_with = "body_file")]
    pub body: Option<String>,
    /// Path to a file containing the PR body. Preferred over --body: the file path is
    /// passed shell-safely, so backticks and $(...) in the body are never evaluated.
    #[arg(long, conflicts_with = "body")]
    pub body_file: Option<String>,
    /// Open the PR as a draft.
    #[arg(long)]
    pub draft: bool,
}

#[derive(Debug, Args)]
pub struct PrSyncArgs {
    /// Sync an entire stack from its root.
    #[arg(long, conflicts_with = "change")]
    pub root: Option<String>,
    /// Sync a single change.
    #[arg(long, conflicts_with = "root")]
    pub change: Option<String>,
}

#[derive(Debug, Args)]
pub struct PrMergeArgs {
    /// Merge a single change PR.
    #[arg(long, conflicts_with = "stack")]
    pub change: Option<String>,
    /// Merge a ready stack from its root.
    #[arg(long, conflicts_with = "change")]
    pub stack: Option<String>,
}

#[derive(Debug, Args)]
pub struct GraphArgs {
    /// Absolute workspace path to inspect.
    #[arg(long)]
    pub workspace: Option<String>,
}

#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// Absolute workspace path to inspect.
    #[arg(long)]
    pub workspace: Option<String>,
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{ChangeCommand, Cli, Command, PrCommand, PrEnsureArgs, PrPushArgs, RepoCommand, WorkspaceCommand};

    #[test]
    fn repo_ensure_matches_phase_a_shape() {
        let cli = Cli::parse_from(["cube", "repo", "ensure", "--origin", "git@github.com:spinyfin/mono.git"]);

        match cli.command {
            Command::Repo {
                command: RepoCommand::Ensure { reponame, origin },
            } => {
                assert!(reponame.is_none());
                assert_eq!(origin.as_deref(), Some("git@github.com:spinyfin/mono.git"));
            }
            _ => panic!("expected repo ensure command"),
        }
    }

    #[test]
    fn repo_ensure_accepts_positional_reponame() {
        let cli = Cli::parse_from(["cube", "repo", "ensure", "bduff"]);

        match cli.command {
            Command::Repo {
                command: RepoCommand::Ensure { reponame, origin },
            } => {
                assert_eq!(reponame.as_deref(), Some("bduff"));
                assert!(origin.is_none());
            }
            _ => panic!("expected repo ensure command"),
        }
    }

    #[test]
    fn repo_ensure_rejects_both_or_neither() {
        let both = Cli::try_parse_from(["cube", "repo", "ensure", "bduff", "--origin", "git@github.com:o/r.git"]);
        assert!(both.is_err());

        let neither = Cli::try_parse_from(["cube", "repo", "ensure"]);
        assert!(neither.is_err());
    }

    #[test]
    fn workspace_lease_matches_docs_shape() {
        let cli = Cli::parse_from(["cube", "workspace", "lease", "mono", "--task", "implement parser"]);

        match cli.command {
            Command::Workspace {
                command:
                    WorkspaceCommand::Lease {
                        repo,
                        task,
                        prefer,
                        allow_dirty,
                        resume_pr,
                    },
            } => {
                assert_eq!(repo, "mono");
                assert_eq!(task, "implement parser");
                assert!(prefer.is_none());
                assert!(!allow_dirty);
                assert!(resume_pr.is_none());
            }
            _ => panic!("expected workspace lease command"),
        }
    }

    #[test]
    fn workspace_lease_accepts_prefer_flag() {
        let cli = Cli::parse_from([
            "cube",
            "workspace",
            "lease",
            "mono",
            "--task",
            "resume parser work",
            "--prefer",
            "mono-agent-007",
        ]);

        match cli.command {
            Command::Workspace {
                command:
                    WorkspaceCommand::Lease {
                        repo,
                        task,
                        prefer,
                        allow_dirty,
                        resume_pr,
                    },
            } => {
                assert_eq!(repo, "mono");
                assert_eq!(task, "resume parser work");
                assert_eq!(prefer.as_deref(), Some("mono-agent-007"));
                assert!(!allow_dirty);
                assert!(resume_pr.is_none());
            }
            _ => panic!("expected workspace lease command"),
        }
    }

    #[test]
    fn workspace_lease_accepts_allow_dirty_with_prefer() {
        let cli = Cli::parse_from([
            "cube",
            "workspace",
            "lease",
            "mono",
            "--task",
            "recover stranded work",
            "--prefer",
            "mono-agent-007",
            "--allow-dirty",
        ]);

        match cli.command {
            Command::Workspace {
                command: WorkspaceCommand::Lease {
                    prefer, allow_dirty, ..
                },
            } => {
                assert_eq!(prefer.as_deref(), Some("mono-agent-007"));
                assert!(allow_dirty);
            }
            _ => panic!("expected workspace lease command"),
        }
    }

    #[test]
    fn workspace_lease_allow_dirty_requires_prefer() {
        let result = Cli::try_parse_from([
            "cube",
            "workspace",
            "lease",
            "mono",
            "--task",
            "recover stranded work",
            "--allow-dirty",
        ]);
        assert!(result.is_err());
    }

    #[test]
    fn workspace_lease_accepts_resume_pr_flag() {
        let cli = Cli::parse_from([
            "cube",
            "workspace",
            "lease",
            "mono",
            "--task",
            "resume PR 1364",
            "--resume-pr",
            "1364",
        ]);

        match cli.command {
            Command::Workspace {
                command:
                    WorkspaceCommand::Lease {
                        repo,
                        task,
                        prefer,
                        allow_dirty,
                        resume_pr,
                    },
            } => {
                assert_eq!(repo, "mono");
                assert_eq!(task, "resume PR 1364");
                assert!(prefer.is_none());
                assert!(!allow_dirty);
                assert_eq!(resume_pr, Some(1364));
            }
            _ => panic!("expected workspace lease command"),
        }
    }

    #[test]
    fn workspace_lease_resume_pr_composes_with_prefer() {
        let cli = Cli::parse_from([
            "cube",
            "workspace",
            "lease",
            "mono",
            "--task",
            "resume PR 42 on warm workspace",
            "--prefer",
            "mono-agent-007",
            "--resume-pr",
            "42",
        ]);

        match cli.command {
            Command::Workspace {
                command:
                    WorkspaceCommand::Lease {
                        prefer,
                        resume_pr,
                        allow_dirty,
                        ..
                    },
            } => {
                assert_eq!(prefer.as_deref(), Some("mono-agent-007"));
                assert_eq!(resume_pr, Some(42));
                assert!(!allow_dirty);
            }
            _ => panic!("expected workspace lease command"),
        }
    }

    #[test]
    fn workspace_lease_resume_pr_conflicts_with_allow_dirty() {
        let result = Cli::try_parse_from([
            "cube",
            "workspace",
            "lease",
            "mono",
            "--task",
            "bad flags",
            "--prefer",
            "mono-agent-007",
            "--allow-dirty",
            "--resume-pr",
            "42",
        ]);
        assert!(
            result.is_err(),
            "--allow-dirty and --resume-pr must be mutually exclusive"
        );
    }

    #[test]
    fn workspace_release_accepts_lease_or_workspace_id() {
        let by_lease = Cli::parse_from(["cube", "workspace", "release", "--lease", "abc-123"]);
        match by_lease.command {
            Command::Workspace {
                command: WorkspaceCommand::Release {
                    workspace, lease, repo, ..
                },
            } => {
                assert!(workspace.is_none());
                assert_eq!(lease.as_deref(), Some("abc-123"));
                assert!(repo.is_none());
            }
            _ => panic!("expected release command"),
        }

        let by_id = Cli::parse_from(["cube", "workspace", "release", "mono-agent-004"]);
        match by_id.command {
            Command::Workspace {
                command: WorkspaceCommand::Release {
                    workspace, lease, repo, ..
                },
            } => {
                assert_eq!(workspace.as_deref(), Some("mono-agent-004"));
                assert!(lease.is_none());
                assert!(repo.is_none());
            }
            _ => panic!("expected release command"),
        }

        let by_id_with_repo = Cli::parse_from(["cube", "workspace", "release", "mono-agent-004", "--repo", "mono"]);
        match by_id_with_repo.command {
            Command::Workspace {
                command: WorkspaceCommand::Release { workspace, repo, .. },
            } => {
                assert_eq!(workspace.as_deref(), Some("mono-agent-004"));
                assert_eq!(repo.as_deref(), Some("mono"));
            }
            _ => panic!("expected release command"),
        }
    }

    #[test]
    fn workspace_release_accepts_reason_and_keep_dirty() {
        let cli = Cli::parse_from([
            "cube",
            "workspace",
            "release",
            "--lease",
            "abc",
            "--reason",
            "crash",
            "--keep-dirty",
        ]);
        match cli.command {
            Command::Workspace {
                command: WorkspaceCommand::Release { reason, keep_dirty, .. },
            } => {
                assert_eq!(reason.as_deref(), Some("crash"));
                assert!(keep_dirty);
            }
            _ => panic!("expected release command"),
        }
    }

    #[test]
    fn workspace_heartbeat_parses() {
        let cli = Cli::parse_from([
            "cube",
            "workspace",
            "heartbeat",
            "--lease",
            "abc-123",
            "--ttl-seconds",
            "600",
        ]);
        match cli.command {
            Command::Workspace {
                command: WorkspaceCommand::Heartbeat { lease, ttl_seconds },
            } => {
                assert_eq!(lease, "abc-123");
                assert_eq!(ttl_seconds, Some(600));
            }
            _ => panic!("expected heartbeat command"),
        }
    }

    #[test]
    fn workspace_force_release_parses_both_forms() {
        let by_id = Cli::parse_from(["cube", "workspace", "force-release", "mono-agent-004"]);
        match by_id.command {
            Command::Workspace {
                command:
                    WorkspaceCommand::ForceRelease {
                        workspace,
                        lease,
                        reason,
                        ..
                    },
            } => {
                assert_eq!(workspace.as_deref(), Some("mono-agent-004"));
                assert!(lease.is_none());
                assert!(reason.is_none());
            }
            _ => panic!("expected force-release command"),
        }

        let by_lease = Cli::parse_from([
            "cube",
            "workspace",
            "force-release",
            "--lease",
            "abc",
            "--reason",
            "stuck",
        ]);
        match by_lease.command {
            Command::Workspace {
                command:
                    WorkspaceCommand::ForceRelease {
                        workspace,
                        lease,
                        reason,
                        ..
                    },
            } => {
                assert!(workspace.is_none());
                assert_eq!(lease.as_deref(), Some("abc"));
                assert_eq!(reason.as_deref(), Some("stuck"));
            }
            _ => panic!("expected force-release command"),
        }
    }

    #[test]
    fn workspace_release_rejects_both_or_neither() {
        // Both forms together
        let both = Cli::try_parse_from(["cube", "workspace", "release", "mono-agent-004", "--lease", "abc-123"]);
        assert!(both.is_err());

        // Neither
        let neither = Cli::try_parse_from(["cube", "workspace", "release"]);
        assert!(neither.is_err());

        // --repo without workspace id is also rejected (requires)
        let lonely_repo = Cli::try_parse_from(["cube", "workspace", "release", "--repo", "mono"]);
        assert!(lonely_repo.is_err());
    }

    #[test]
    fn workspace_list_accepts_filters() {
        let cli = Cli::parse_from([
            "cube",
            "workspace",
            "list",
            "--repo",
            "mono",
            "--state",
            "leased",
            "--holder",
            "boss/*",
        ]);

        match cli.command {
            Command::Workspace {
                command: WorkspaceCommand::List { repo, state, holder },
            } => {
                assert_eq!(repo.as_deref(), Some("mono"));
                assert_eq!(state.as_deref(), Some("leased"));
                assert_eq!(holder.as_deref(), Some("boss/*"));
            }
            _ => panic!("expected workspace list command"),
        }
    }

    #[test]
    fn workspace_list_with_no_flags_is_global() {
        let cli = Cli::parse_from(["cube", "workspace", "list"]);
        match cli.command {
            Command::Workspace {
                command: WorkspaceCommand::List { repo, state, holder },
            } => {
                assert!(repo.is_none());
                assert!(state.is_none());
                assert!(holder.is_none());
            }
            _ => panic!("expected workspace list command"),
        }
    }

    #[test]
    fn workspace_remove_parses_basic_form() {
        let cli = Cli::parse_from(["cube", "workspace", "remove", "mono-agent-004"]);
        match cli.command {
            Command::Workspace {
                command:
                    WorkspaceCommand::Remove {
                        workspace,
                        repo,
                        force,
                        expunge,
                    },
            } => {
                assert_eq!(workspace, "mono-agent-004");
                assert!(repo.is_none());
                assert!(!force);
                assert!(!expunge);
            }
            _ => panic!("expected workspace remove command"),
        }
    }

    #[test]
    fn workspace_remove_accepts_repo_and_force() {
        let cli = Cli::parse_from([
            "cube",
            "workspace",
            "remove",
            "mono-agent-004",
            "--repo",
            "mono",
            "--force",
        ]);
        match cli.command {
            Command::Workspace {
                command:
                    WorkspaceCommand::Remove {
                        workspace,
                        repo,
                        force,
                        expunge,
                    },
            } => {
                assert_eq!(workspace, "mono-agent-004");
                assert_eq!(repo.as_deref(), Some("mono"));
                assert!(force);
                assert!(!expunge);
            }
            _ => panic!("expected workspace remove command"),
        }
    }

    #[test]
    fn workspace_remove_accepts_expunge_flag() {
        let cli = Cli::parse_from(["cube", "workspace", "remove", "mono-agent-004", "--expunge"]);
        match cli.command {
            Command::Workspace {
                command:
                    WorkspaceCommand::Remove {
                        workspace,
                        force,
                        expunge,
                        ..
                    },
            } => {
                assert_eq!(workspace, "mono-agent-004");
                assert!(!force);
                assert!(expunge);
            }
            _ => panic!("expected workspace remove command"),
        }
    }

    #[test]
    fn workspace_remove_requires_workspace_id() {
        let result = Cli::try_parse_from(["cube", "workspace", "remove"]);
        assert!(result.is_err());
    }

    #[test]
    fn workspace_gc_parses_default() {
        let cli = Cli::parse_from(["cube", "workspace", "gc"]);
        match cli.command {
            Command::Workspace {
                command: WorkspaceCommand::Gc { workspace, dry_run },
            } => {
                assert!(workspace.is_none());
                assert!(!dry_run);
            }
            _ => panic!("expected workspace gc command"),
        }
    }

    #[test]
    fn workspace_gc_parses_with_workspace_and_dry_run() {
        let cli = Cli::parse_from(["cube", "workspace", "gc", "--workspace", "mono-agent-001", "--dry-run"]);
        match cli.command {
            Command::Workspace {
                command: WorkspaceCommand::Gc { workspace, dry_run },
            } => {
                assert_eq!(workspace.as_deref(), Some("mono-agent-001"));
                assert!(dry_run);
            }
            _ => panic!("expected workspace gc command"),
        }
    }

    #[test]
    fn change_create_accepts_workspace_or_parent() {
        let cli = Cli::parse_from([
            "cube",
            "change",
            "create",
            "--workspace",
            "/ws/mono-agent-007",
            "--title",
            "Add parser model",
        ]);

        match cli.command {
            Command::Change {
                command: ChangeCommand::Create(args),
            } => {
                assert_eq!(args.workspace.as_deref(), Some("/ws/mono-agent-007"));
                assert_eq!(args.parent, None);
                assert_eq!(args.title, "Add parser model");
            }
            _ => panic!("expected change create command"),
        }
    }

    #[test]
    fn pr_merge_requires_one_selector() {
        let cli = Cli::parse_from(["cube", "pr", "merge", "--change", "chg_a"]);

        match cli.command {
            Command::Pr {
                command: PrCommand::Merge(args),
            } => {
                assert_eq!(args.change.as_deref(), Some("chg_a"));
                assert_eq!(args.stack, None);
            }
            _ => panic!("expected pr merge command"),
        }
    }

    #[test]
    fn pr_ensure_accepts_all_flags() {
        let cli = Cli::parse_from([
            "cube",
            "pr",
            "ensure",
            "--branch",
            "boss/exec_abc123_01",
            "--title",
            "My PR",
            "--body",
            "A description",
            "--draft",
        ]);

        match cli.command {
            Command::Pr {
                command:
                    PrCommand::Ensure(PrEnsureArgs {
                        branch,
                        title,
                        body,
                        body_file,
                        draft,
                    }),
            } => {
                assert_eq!(branch.as_deref(), Some("boss/exec_abc123_01"));
                assert_eq!(title.as_deref(), Some("My PR"));
                assert_eq!(body.as_deref(), Some("A description"));
                assert!(body_file.is_none());
                assert!(draft);
            }
            _ => panic!("expected pr ensure command"),
        }
    }

    #[test]
    fn pr_ensure_branch_is_optional() {
        let cli = Cli::parse_from(["cube", "pr", "ensure"]);

        match cli.command {
            Command::Pr {
                command:
                    PrCommand::Ensure(PrEnsureArgs {
                        branch,
                        title,
                        body,
                        body_file,
                        draft,
                    }),
            } => {
                assert!(branch.is_none());
                assert!(title.is_none());
                assert!(body.is_none());
                assert!(body_file.is_none());
                assert!(!draft);
            }
            _ => panic!("expected pr ensure command"),
        }
    }

    #[test]
    fn pr_ensure_accepts_body_file_flag() {
        let cli = Cli::parse_from([
            "cube",
            "pr",
            "ensure",
            "--branch",
            "boss/exec_abc123_01",
            "--title",
            "My PR",
            "--body-file",
            "/tmp/pr-body.md",
        ]);

        match cli.command {
            Command::Pr {
                command:
                    PrCommand::Ensure(PrEnsureArgs {
                        branch,
                        title,
                        body,
                        body_file,
                        draft,
                    }),
            } => {
                assert_eq!(branch.as_deref(), Some("boss/exec_abc123_01"));
                assert_eq!(title.as_deref(), Some("My PR"));
                assert!(body.is_none());
                assert_eq!(body_file.as_deref(), Some("/tmp/pr-body.md"));
                assert!(!draft);
            }
            _ => panic!("expected pr ensure command"),
        }
    }

    #[test]
    fn pr_push_parses_explicit_pr_and_branch() {
        let cli = Cli::parse_from(["cube", "pr", "push", "--pr", "42", "--branch", "boss/exec_abc123_01"]);
        match cli.command {
            Command::Pr {
                command:
                    PrCommand::Push(PrPushArgs {
                        pr,
                        branch,
                        force_with_lease,
                    }),
            } => {
                assert_eq!(pr, Some(42));
                assert_eq!(branch.as_deref(), Some("boss/exec_abc123_01"));
                assert!(!force_with_lease);
            }
            _ => panic!("expected pr push command"),
        }
    }

    #[test]
    fn pr_push_accepts_force_with_lease() {
        let cli = Cli::parse_from([
            "cube",
            "pr",
            "push",
            "--pr",
            "42",
            "--branch",
            "boss/exec_abc",
            "--force-with-lease",
        ]);
        match cli.command {
            Command::Pr {
                command:
                    PrCommand::Push(PrPushArgs {
                        pr,
                        branch,
                        force_with_lease,
                    }),
            } => {
                assert_eq!(pr, Some(42));
                assert_eq!(branch.as_deref(), Some("boss/exec_abc"));
                assert!(force_with_lease);
            }
            _ => panic!("expected pr push command"),
        }
    }

    #[test]
    fn pr_push_all_args_optional() {
        let cli = Cli::parse_from(["cube", "pr", "push"]);
        match cli.command {
            Command::Pr {
                command:
                    PrCommand::Push(PrPushArgs {
                        pr,
                        branch,
                        force_with_lease,
                    }),
            } => {
                assert!(pr.is_none());
                assert!(branch.is_none());
                assert!(!force_with_lease);
            }
            _ => panic!("expected pr push command"),
        }
    }
}
