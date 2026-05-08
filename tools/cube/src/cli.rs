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
    /// Resolve or materialize a repo pool from its origin URL.
    Ensure {
        /// Origin URL for the repo.
        #[arg(long)]
        origin: String,
    },
    /// Add or update repo pool configuration.
    Add {
        /// Stable repo identifier such as `mono`.
        repo: String,
        /// Origin URL for the repo.
        #[arg(long)]
        origin: String,
        /// Integration branch name.
        #[arg(long, default_value = "main")]
        main_branch: String,
        /// Root directory containing reusable workspaces.
        #[arg(long)]
        workspace_root: String,
        /// Shared prefix for workspaces in the pool.
        #[arg(long)]
        workspace_prefix: String,
        /// Optional source path used for future workspace creation.
        #[arg(long)]
        source: Option<String>,
    },
    /// List known repo pools.
    List,
    /// Show repo pool configuration.
    Info {
        /// Stable repo identifier such as `mono`.
        repo: String,
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
    /// Remove a workspace row from the registry.
    ///
    /// Deletes the `workspaces` row (and cascades `workspace_setup`)
    /// for the given workspace id. The on-disk workspace directory is
    /// left untouched — it may already be gone, or the operator may
    /// want to inspect it. Use this to clean up dangling registry rows
    /// after a workspace directory has been wiped manually.
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
    /// Sync local change state to GitHub pull requests.
    Sync(PrSyncArgs),
    /// Merge one PR or a ready sub-stack.
    Merge(PrMergeArgs),
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

    use super::{ChangeCommand, Cli, Command, PrCommand, RepoCommand, WorkspaceCommand};

    #[test]
    fn repo_ensure_matches_phase_a_shape() {
        let cli = Cli::parse_from([
            "cube",
            "repo",
            "ensure",
            "--origin",
            "git@github.com:spinyfin/mono.git",
        ]);

        match cli.command {
            Command::Repo {
                command: RepoCommand::Ensure { origin },
            } => {
                assert_eq!(origin, "git@github.com:spinyfin/mono.git");
            }
            _ => panic!("expected repo ensure command"),
        }
    }

    #[test]
    fn workspace_lease_matches_docs_shape() {
        let cli = Cli::parse_from([
            "cube",
            "workspace",
            "lease",
            "mono",
            "--task",
            "implement parser",
        ]);

        match cli.command {
            Command::Workspace {
                command:
                    WorkspaceCommand::Lease {
                        repo,
                        task,
                        prefer,
                    },
            } => {
                assert_eq!(repo, "mono");
                assert_eq!(task, "implement parser");
                assert!(prefer.is_none());
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
                    },
            } => {
                assert_eq!(repo, "mono");
                assert_eq!(task, "resume parser work");
                assert_eq!(prefer.as_deref(), Some("mono-agent-007"));
            }
            _ => panic!("expected workspace lease command"),
        }
    }

    #[test]
    fn workspace_release_accepts_lease_or_workspace_id() {
        let by_lease = Cli::parse_from(["cube", "workspace", "release", "--lease", "abc-123"]);
        match by_lease.command {
            Command::Workspace {
                command:
                    WorkspaceCommand::Release {
                        workspace,
                        lease,
                        repo,
                        ..
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
                command:
                    WorkspaceCommand::Release {
                        workspace,
                        lease,
                        repo,
                        ..
                    },
            } => {
                assert_eq!(workspace.as_deref(), Some("mono-agent-004"));
                assert!(lease.is_none());
                assert!(repo.is_none());
            }
            _ => panic!("expected release command"),
        }

        let by_id_with_repo = Cli::parse_from([
            "cube", "workspace", "release", "mono-agent-004", "--repo", "mono",
        ]);
        match by_id_with_repo.command {
            Command::Workspace {
                command:
                    WorkspaceCommand::Release { workspace, repo, .. },
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
                command:
                    WorkspaceCommand::Release {
                        reason, keep_dirty, ..
                    },
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
        let both = Cli::try_parse_from([
            "cube",
            "workspace",
            "release",
            "mono-agent-004",
            "--lease",
            "abc-123",
        ]);
        assert!(both.is_err());

        // Neither
        let neither = Cli::try_parse_from(["cube", "workspace", "release"]);
        assert!(neither.is_err());

        // --repo without workspace id is also rejected (requires)
        let lonely_repo =
            Cli::try_parse_from(["cube", "workspace", "release", "--repo", "mono"]);
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
                command:
                    WorkspaceCommand::List {
                        repo,
                        state,
                        holder,
                    },
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
                command:
                    WorkspaceCommand::List {
                        repo,
                        state,
                        holder,
                    },
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
                    },
            } => {
                assert_eq!(workspace, "mono-agent-004");
                assert!(repo.is_none());
                assert!(!force);
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
                    },
            } => {
                assert_eq!(workspace, "mono-agent-004");
                assert_eq!(repo.as_deref(), Some("mono"));
                assert!(force);
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
}
