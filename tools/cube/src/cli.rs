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
    },
    /// Release a workspace lease.
    Release {
        /// Lease id returned by `workspace lease`.
        #[arg(long)]
        lease: String,
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
                command: WorkspaceCommand::Lease { repo, task },
            } => {
                assert_eq!(repo, "mono");
                assert_eq!(task, "implement parser");
            }
            _ => panic!("expected workspace lease command"),
        }
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
