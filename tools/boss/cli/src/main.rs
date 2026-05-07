use std::io::{self, IsTerminal, Write};
use std::process::ExitCode;

use anyhow::Result;
use boss_client::{
    BossClient, Discovery, engine_socket_reachable, ensure_engine_running, running_engine_pid,
    stop_engine,
};
use boss_protocol::{
    CreateChoreInput, CreateProductInput, CreateProjectInput, CreateTaskInput, FrontendEvent,
    FrontendRequest, Product, Project, Task, WorkItem, WorkItemPatch,
};
use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use comfy_table::{ContentArrangement, Table};
use serde::Serialize;

#[derive(Debug, Parser)]
#[command(name = "boss", about = "Boss work CLI")]
struct Cli {
    #[command(flatten)]
    global: GlobalFlags,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Clone, Args)]
struct GlobalFlags {
    #[arg(long, global = true)]
    json: bool,

    #[arg(long, global = true)]
    quiet: bool,

    #[arg(long, global = true)]
    no_input: bool,

    /// Suppress autostart side effects.
    ///
    /// Two effects, both off-by-default:
    ///   1. The CLI will not transparently start the engine when
    ///      its socket is unreachable.
    ///   2. `boss task create` / `boss chore create` create the work
    ///      item but the engine will NOT auto-dispatch a worker for
    ///      it. The new chore/task stays in the `todo` column until
    ///      something explicitly schedules it (`bossctl work start
    ///      <id>` or a kanban drag-to-Doing).
    #[arg(long, global = true)]
    no_autostart: bool,

    #[arg(long, global = true)]
    socket_path: Option<String>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Print authoritative Boss CLI reference documentation.
    Reference,
    Product {
        #[command(subcommand)]
        command: ProductCommand,
    },
    Project {
        #[command(subcommand)]
        command: ProjectCommand,
    },
    Task {
        #[command(subcommand)]
        command: TaskCommand,
    },
    Chore {
        #[command(subcommand)]
        command: ChoreCommand,
    },
    Engine {
        #[command(subcommand)]
        command: EngineCommand,
    },
}

#[derive(Debug, Subcommand)]
enum ProductCommand {
    Create(ProductCreateArgs),
    List,
    Show(ProductSelectorArg),
    Update(ProductUpdateArgs),
    /// Archive a product. Products are not hard-deleted; the engine convention
    /// is to set status=archived so the row stays available for history.
    Delete(ProductSelectorArg),
    /// Move a product into a different lifecycle status (active/paused/archived).
    Move(ProductMoveArgs),
}

#[derive(Debug, Subcommand)]
enum ProjectCommand {
    Create(ProjectCreateArgs),
    List(ProjectListArgs),
    Show(ProjectShowArgs),
    Update(ProjectUpdateArgs),
    /// Archive a project. Projects are not hard-deleted; the engine convention
    /// is to set status=archived so the row stays available for history.
    Delete(ProjectSelectorArgs),
    /// Move a project into a different lifecycle status
    /// (planned/active/blocked/done/archived).
    Move(ProjectMoveArgs),
}

#[derive(Debug, Subcommand)]
enum TaskCommand {
    Create(TaskCreateArgs),
    List(TaskListArgs),
    Show(TaskIdArg),
    Update(TaskUpdateArgs),
    Move(TaskMoveArgs),
    Delete(TaskDeleteArgs),
    Reorder(TaskReorderArgs),
}

#[derive(Debug, Subcommand)]
enum ChoreCommand {
    Create(ChoreCreateArgs),
    List(ChoreListArgs),
    Show(TaskIdArg),
    Update(TaskUpdateArgs),
    Move(TaskMoveArgs),
    Delete(TaskDeleteArgs),
}

#[derive(Debug, Subcommand)]
enum EngineCommand {
    Status,
    Start,
    Stop,
}

#[derive(Debug, Clone, Args)]
struct ProductSelectorArg {
    selector: String,
}

#[derive(Debug, Clone, Args)]
struct ProjectSelectorArgs {
    #[arg(long)]
    product: Option<String>,

    selector: String,
}

#[derive(Debug, Clone, Args)]
struct ProductScopedArgs {
    #[arg(long)]
    product: Option<String>,
}

#[derive(Debug, Clone, Args)]
struct ProductCreateArgs {
    #[arg(long)]
    name: Option<String>,

    #[arg(long)]
    description: Option<String>,

    #[arg(long = "repo")]
    #[arg(alias = "repo-remote-url")]
    repo_remote_url: Option<String>,
}

#[derive(Debug, Clone, Args)]
struct ProductUpdateArgs {
    selector: String,

    #[arg(long)]
    name: Option<String>,

    #[arg(long)]
    description: Option<String>,

    #[arg(long = "repo")]
    #[arg(alias = "repo-remote-url")]
    repo_remote_url: Option<String>,

    #[arg(long)]
    status: Option<ProductStatus>,
}

#[derive(Debug, Clone, Args)]
struct ProjectCreateArgs {
    #[arg(long)]
    product: Option<String>,

    #[arg(long)]
    name: Option<String>,

    #[arg(long)]
    description: Option<String>,

    #[arg(long)]
    goal: Option<String>,
}

#[derive(Debug, Clone, Args)]
struct ProjectListArgs {
    #[arg(long)]
    product: Option<String>,

    /// Filter by status. Repeat the flag or use a comma-separated list.
    #[arg(long, value_delimiter = ',')]
    status: Vec<ProjectStatus>,

    /// Case-insensitive substring match against name and description.
    #[arg(long = "match")]
    match_term: Option<String>,

    /// Cap the number of returned rows (applied after filtering).
    #[arg(long)]
    limit: Option<usize>,

    /// Filter to specific id(s); repeatable.
    #[arg(long)]
    id: Vec<String>,
}

#[derive(Debug, Clone, Args)]
struct ProjectShowArgs {
    #[arg(long)]
    product: Option<String>,

    selector: String,
}

#[derive(Debug, Clone, Args)]
struct ProjectUpdateArgs {
    #[arg(long)]
    product: Option<String>,

    selector: String,

    #[arg(long)]
    name: Option<String>,

    #[arg(long)]
    description: Option<String>,

    #[arg(long)]
    goal: Option<String>,

    #[arg(long)]
    status: Option<ProjectStatus>,

    #[arg(long)]
    priority: Option<ProjectPriority>,
}

#[derive(Debug, Clone, Args)]
struct TaskCreateArgs {
    #[arg(long)]
    product: Option<String>,

    #[arg(long)]
    project: Option<String>,

    #[arg(long)]
    name: Option<String>,

    #[arg(long)]
    description: Option<String>,
}

#[derive(Debug, Clone, Args)]
struct TaskListArgs {
    #[arg(long)]
    product: Option<String>,

    #[arg(long)]
    project: Option<String>,
}

#[derive(Debug, Clone, Args)]
struct ChoreCreateArgs {
    #[arg(long)]
    product: Option<String>,

    #[arg(long)]
    name: Option<String>,

    #[arg(long)]
    description: Option<String>,
}

#[derive(Debug, Clone, Args)]
struct ChoreListArgs {
    #[arg(long)]
    product: Option<String>,

    /// Filter by status. Repeat the flag or use a comma-separated list.
    #[arg(long, value_delimiter = ',')]
    status: Vec<TaskStatus>,

    /// Case-insensitive substring match against name and description.
    #[arg(long = "match")]
    match_term: Option<String>,

    /// Cap the number of returned rows (applied after filtering).
    #[arg(long)]
    limit: Option<usize>,

    /// Filter to specific id(s); repeatable.
    #[arg(long)]
    id: Vec<String>,
}

#[derive(Debug, Clone, Args)]
struct TaskIdArg {
    id: String,
}

#[derive(Debug, Clone, Args)]
struct TaskUpdateArgs {
    id: String,

    #[arg(long)]
    name: Option<String>,

    #[arg(long)]
    description: Option<String>,

    #[arg(long)]
    status: Option<TaskStatus>,

    #[arg(long)]
    ordinal: Option<i64>,

    #[arg(long = "pr-url")]
    pr_url: Option<String>,
}

#[derive(Debug, Clone, Args)]
struct TaskMoveArgs {
    id: String,

    #[arg(long = "to")]
    target: MoveTarget,
}

#[derive(Debug, Clone, Args)]
struct ProductMoveArgs {
    selector: String,

    #[arg(long = "to")]
    target: ProductStatus,
}

#[derive(Debug, Clone, Args)]
struct ProjectMoveArgs {
    #[arg(long)]
    product: Option<String>,

    selector: String,

    #[arg(long = "to")]
    target: ProjectStatus,
}

#[derive(Debug, Clone, Args)]
struct TaskDeleteArgs {
    id: String,
}

#[derive(Debug, Clone, Args)]
struct TaskReorderArgs {
    #[arg(long)]
    product: Option<String>,

    #[arg(long)]
    project: Option<String>,

    #[arg(long, value_delimiter = ',')]
    ids: Vec<String>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ProductStatus {
    Active,
    Paused,
    Archived,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ProjectStatus {
    Planned,
    Active,
    Blocked,
    Done,
    Archived,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ProjectPriority {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum TaskStatus {
    Todo,
    Active,
    Blocked,
    InReview,
    Done,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum MoveTarget {
    Backlog,
    Doing,
    Review,
    Done,
    Todo,
    Active,
    Blocked,
    InReview,
}

impl ProductStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Archived => "archived",
        }
    }
}

impl ProjectStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Planned => "planned",
            Self::Active => "active",
            Self::Blocked => "blocked",
            Self::Done => "done",
            Self::Archived => "archived",
        }
    }
}

impl ProjectPriority {
    fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

impl TaskStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Todo => "todo",
            Self::Active => "active",
            Self::Blocked => "blocked",
            Self::InReview => "in_review",
            Self::Done => "done",
        }
    }
}

impl MoveTarget {
    fn as_status(self) -> &'static str {
        match self {
            Self::Backlog | Self::Todo => "todo",
            Self::Doing | Self::Active => "active",
            Self::Review | Self::InReview => "in_review",
            Self::Done => "done",
            Self::Blocked => "blocked",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OutputMode {
    Human,
    Json,
}

#[derive(Debug, Serialize)]
struct CliReferenceDocument {
    cli: &'static str,
    usage_rules: Vec<&'static str>,
    selector_semantics: Vec<&'static str>,
    status_semantics: Vec<&'static str>,
    workflow_guidance: Vec<&'static str>,
    commands: Vec<CliReferenceSection>,
}

#[derive(Debug, Serialize)]
struct CliReferenceSection {
    path: String,
    help: String,
}

#[derive(Debug)]
enum CliError {
    Usage(String),
    NotFound(String),
    Conflict(String),
    EngineUnavailable(String),
    Application(String),
    Internal(anyhow::Error),
}

impl CliError {
    fn internal(err: impl Into<anyhow::Error>) -> Self {
        Self::Internal(err.into())
    }

    fn usage(message: impl Into<String>) -> Self {
        Self::Usage(message.into())
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self::NotFound(message.into())
    }

    fn conflict(message: impl Into<String>) -> Self {
        Self::Conflict(message.into())
    }

    fn engine_unavailable(message: impl Into<String>) -> Self {
        Self::EngineUnavailable(message.into())
    }

    fn application(message: impl Into<String>) -> Self {
        Self::Application(message.into())
    }

    fn exit_code(&self) -> ExitCode {
        match self {
            Self::Usage(_) => ExitCode::from(2),
            Self::NotFound(_) => ExitCode::from(3),
            Self::Conflict(_) => ExitCode::from(4),
            Self::EngineUnavailable(_) => ExitCode::from(5),
            Self::Application(_) => ExitCode::from(6),
            Self::Internal(_) => ExitCode::from(7),
        }
    }
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Usage(message)
            | Self::NotFound(message)
            | Self::Conflict(message)
            | Self::EngineUnavailable(message)
            | Self::Application(message) => f.write_str(message),
            Self::Internal(err) => write!(f, "{err:#}"),
        }
    }
}

struct RunContext {
    output_mode: OutputMode,
    quiet: bool,
    allow_input: bool,
    discovery: Discovery,
    /// Mirror of the global `--no-autostart` flag. Today this also
    /// gates per-work-item auto-dispatch (`boss chore create
    /// --no-autostart` → engine creates the chore in `todo` but does
    /// not spin up a worker for it).
    no_autostart: bool,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run_cli(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            err.exit_code()
        }
    }
}

async fn run_cli(cli: Cli) -> Result<(), CliError> {
    match cli.command {
        Commands::Reference => run_reference_command(&cli.global),
        Commands::Product { command } => {
            let ctx = RunContext::from_flags(&cli.global)?;
            run_product_command(command, &ctx).await
        }
        Commands::Project { command } => {
            let ctx = RunContext::from_flags(&cli.global)?;
            run_project_command(command, &ctx).await
        }
        Commands::Task { command } => {
            let ctx = RunContext::from_flags(&cli.global)?;
            run_task_command(command, &ctx).await
        }
        Commands::Chore { command } => {
            let ctx = RunContext::from_flags(&cli.global)?;
            run_chore_command(command, &ctx).await
        }
        Commands::Engine { command } => {
            let ctx = RunContext::from_flags(&cli.global)?;
            run_engine_command(command, &ctx).await
        }
    }
}

fn run_reference_command(flags: &GlobalFlags) -> Result<(), CliError> {
    let output_mode = if flags.json {
        OutputMode::Json
    } else {
        OutputMode::Human
    };
    let reference = build_cli_reference()?;

    match output_mode {
        OutputMode::Human => print_cli_reference_human(&reference).map_err(CliError::internal)?,
        OutputMode::Json => {
            serde_json::to_writer_pretty(io::stdout().lock(), &reference)
                .map_err(CliError::internal)?;
            println!();
        }
    }

    Ok(())
}

fn build_cli_reference() -> Result<CliReferenceDocument, CliError> {
    let command = Cli::command().color(clap::ColorChoice::Never);
    let mut commands = Vec::new();
    collect_cli_reference_sections(command, Vec::new(), &mut commands)?;

    Ok(CliReferenceDocument {
        cli: "boss",
        usage_rules: vec![
            "For agent use, prefer non-interactive commands with --json --no-input.",
            "Treat this reference output as the authoritative current CLI surface for this build.",
            "Do not use boss ... --help for syntax discovery when this reference is available.",
            "Omit --socket-path unless you explicitly need a non-default socket.",
            "Omit --no-autostart unless you explicitly need to forbid engine startup or auto-dispatch on `task create` / `chore create`.",
        ],
        selector_semantics: vec![
            "Product selectors accept a product id, slug, or 1-based interactive index. For agent use, prefer slug or id, not numeric indexes.",
            "Project selectors accept a project id, slug, or 1-based interactive index within the selected product. For agent use, prefer slug or id, not numeric indexes.",
            "Task and chore commands that operate on an existing item use the item id, not slug.",
        ],
        status_semantics: vec![
            "CLI status values use in-review on the command line.",
            "Internally, in-review maps to in_review.",
            "Task and chore move targets map: backlog|todo -> todo, doing|active -> active, review|in-review -> in_review, blocked -> blocked, done -> done.",
            "Product move/delete: --to active|paused|archived. delete is a soft archive (sets status=archived).",
            "Project move/delete: --to planned|active|blocked|done|archived. delete is a soft archive (sets status=archived).",
        ],
        workflow_guidance: vec![
            "Use the current UI or conversational context first when deciding where new work belongs.",
            "If you need to compare against existing projects in a product, use boss project list --product <product-selector> --json --no-input.",
            "If the work fits an existing project, create a task in that project.",
            "If it does not fit an existing project and is small and self-contained, create a chore.",
            "If it does not fit an existing project and is broad, ambiguous, investigative, or multi-stage, create a project.",
        ],
        commands,
    })
}

fn collect_cli_reference_sections(
    command: clap::Command,
    path: Vec<String>,
    sections: &mut Vec<CliReferenceSection>,
) -> Result<(), CliError> {
    let mut current_path = path;
    current_path.push(command.get_name().to_owned());

    sections.push(CliReferenceSection {
        path: current_path.join(" "),
        help: render_command_help(command.clone())?,
    });

    for subcommand in command.get_subcommands() {
        collect_cli_reference_sections(subcommand.clone(), current_path.clone(), sections)?;
    }

    Ok(())
}

fn render_command_help(mut command: clap::Command) -> Result<String, CliError> {
    command = command.color(clap::ColorChoice::Never);
    let mut buffer = Vec::new();
    command
        .write_long_help(&mut buffer)
        .map_err(CliError::internal)?;
    let help = String::from_utf8(buffer).map_err(CliError::internal)?;
    Ok(help.trim().to_owned())
}

fn print_cli_reference_human(reference: &CliReferenceDocument) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "Boss CLI reference:")?;
    writeln!(stdout)?;
    print_reference_list(&mut stdout, "General rules", &reference.usage_rules)?;
    print_reference_list(
        &mut stdout,
        "Selector semantics",
        &reference.selector_semantics,
    )?;
    print_reference_list(&mut stdout, "Status semantics", &reference.status_semantics)?;
    print_reference_list(
        &mut stdout,
        "Workflow guidance",
        &reference.workflow_guidance,
    )?;
    writeln!(stdout, "Command help:")?;
    for section in &reference.commands {
        writeln!(stdout, "[{}]", section.path)?;
        writeln!(stdout, "{}", section.help)?;
        writeln!(stdout)?;
    }
    Ok(())
}

fn print_reference_list(writer: &mut impl Write, title: &str, items: &[&str]) -> io::Result<()> {
    writeln!(writer, "{title}:")?;
    for item in items {
        writeln!(writer, "- {item}")?;
    }
    writeln!(writer)?;
    Ok(())
}

impl RunContext {
    fn from_flags(flags: &GlobalFlags) -> Result<Self, CliError> {
        let allow_input =
            !flags.no_input && io::stdin().is_terminal() && io::stdout().is_terminal();
        let discovery = Discovery::from_env(flags.socket_path.as_deref())
            .map_err(CliError::internal)?
            .with_autostart(!flags.no_autostart);

        Ok(Self {
            output_mode: if flags.json {
                OutputMode::Json
            } else {
                OutputMode::Human
            },
            quiet: flags.quiet,
            allow_input,
            discovery,
            no_autostart: flags.no_autostart,
        })
    }
}

async fn run_product_command(command: ProductCommand, ctx: &RunContext) -> Result<(), CliError> {
    let mut client = connect_for_work(ctx).await?;
    match command {
        ProductCommand::Create(args) => {
            let name = required_text(args.name, "Product name", ctx)?;
            let description = optional_text(args.description, "Description", ctx)?;
            let repo_remote_url = optional_text(args.repo_remote_url, "Repo remote URL", ctx)?;

            let product = create_product(
                &mut client,
                CreateProductInput {
                    name,
                    description,
                    repo_remote_url,
                },
            )
            .await?;

            print_entity(ctx, &serde_json::json!({ "product": product }), || {
                print_product_details("Created product", &product);
            })
        }
        ProductCommand::List => {
            let products = list_products(&mut client).await?;
            print_entity(ctx, &serde_json::json!({ "products": products }), || {
                print_products_table(&products);
            })
        }
        ProductCommand::Show(args) => {
            let product = resolve_product(&mut client, Some(args.selector), ctx).await?;
            print_entity(ctx, &serde_json::json!({ "product": product }), || {
                print_product_details("Product", &product);
            })
        }
        ProductCommand::Update(args) => {
            let product = resolve_product(&mut client, Some(args.selector), ctx).await?;
            let patch = WorkItemPatch {
                name: args.name,
                description: args.description,
                status: args.status.map(|status| status.as_str().to_owned()),
                repo_remote_url: args.repo_remote_url,
                ..WorkItemPatch::default()
            };
            ensure_patch_present(
                &patch,
                "provide at least one field to update, such as --name or --status",
            )?;
            let item = update_work_item(&mut client, &product.id, patch).await?;
            let product = expect_product(item)?;
            print_entity(ctx, &serde_json::json!({ "product": product }), || {
                print_product_details("Updated product", &product);
            })
        }
        ProductCommand::Delete(args) => {
            let product = resolve_product(&mut client, Some(args.selector), ctx).await?;
            let patch = WorkItemPatch {
                status: Some(ProductStatus::Archived.as_str().to_owned()),
                ..WorkItemPatch::default()
            };
            let archived = expect_product(update_work_item(&mut client, &product.id, patch).await?)?;
            print_entity(
                ctx,
                &serde_json::json!({
                    "product": archived,
                    "deleted": true,
                    "archived": true,
                }),
                || {
                    if !ctx.quiet {
                        println!(
                            "Archived product {} ({}) — products are not hard-deleted.",
                            archived.name, archived.slug,
                        );
                    }
                },
            )
        }
        ProductCommand::Move(args) => {
            let product = resolve_product(&mut client, Some(args.selector), ctx).await?;
            let patch = WorkItemPatch {
                status: Some(args.target.as_str().to_owned()),
                ..WorkItemPatch::default()
            };
            let moved = expect_product(update_work_item(&mut client, &product.id, patch).await?)?;
            print_entity(ctx, &serde_json::json!({ "product": moved }), || {
                print_product_details("Moved product", &moved);
            })
        }
    }
}

async fn run_project_command(command: ProjectCommand, ctx: &RunContext) -> Result<(), CliError> {
    let mut client = connect_for_work(ctx).await?;
    match command {
        ProjectCommand::Create(args) => {
            let product = resolve_product(&mut client, args.product, ctx).await?;
            let name = required_text(args.name, "Project name", ctx)?;
            let description = optional_text(args.description, "Description", ctx)?;
            let goal = optional_text(args.goal, "Goal", ctx)?;

            let project = create_project(
                &mut client,
                CreateProjectInput {
                    product_id: product.id,
                    name,
                    description,
                    goal,
                },
            )
            .await?;

            print_entity(ctx, &serde_json::json!({ "project": project }), || {
                print_project_details("Created project", &project);
            })
        }
        ProjectCommand::List(args) => {
            let product = resolve_product(&mut client, args.product, ctx).await?;
            let projects = list_projects(&mut client, &product.id).await?;
            let projects = apply_project_list_filters(
                projects,
                &args.status,
                args.match_term.as_deref(),
                &args.id,
                args.limit,
            );
            print_entity(
                ctx,
                &serde_json::json!({ "product": product, "projects": projects }),
                || print_projects_table(&projects),
            )
        }
        ProjectCommand::Show(args) => {
            let product = resolve_product(&mut client, args.product, ctx).await?;
            let project =
                resolve_project(&mut client, &product.id, Some(args.selector), ctx).await?;
            print_entity(ctx, &serde_json::json!({ "project": project }), || {
                print_project_details("Project", &project);
            })
        }
        ProjectCommand::Update(args) => {
            let product = resolve_product(&mut client, args.product, ctx).await?;
            let project =
                resolve_project(&mut client, &product.id, Some(args.selector), ctx).await?;
            let patch = WorkItemPatch {
                name: args.name,
                description: args.description,
                goal: args.goal,
                status: args.status.map(|status| status.as_str().to_owned()),
                priority: args.priority.map(|priority| priority.as_str().to_owned()),
                ..WorkItemPatch::default()
            };
            ensure_patch_present(
                &patch,
                "provide at least one field to update, such as --goal or --priority",
            )?;
            let item = update_work_item(&mut client, &project.id, patch).await?;
            let project = expect_project(item)?;
            print_entity(ctx, &serde_json::json!({ "project": project }), || {
                print_project_details("Updated project", &project);
            })
        }
        ProjectCommand::Delete(args) => {
            let product = resolve_product(&mut client, args.product, ctx).await?;
            let project =
                resolve_project(&mut client, &product.id, Some(args.selector), ctx).await?;
            let patch = WorkItemPatch {
                status: Some(ProjectStatus::Archived.as_str().to_owned()),
                ..WorkItemPatch::default()
            };
            let archived = expect_project(update_work_item(&mut client, &project.id, patch).await?)?;
            print_entity(
                ctx,
                &serde_json::json!({
                    "project": archived,
                    "deleted": true,
                    "archived": true,
                }),
                || {
                    if !ctx.quiet {
                        println!(
                            "Archived project {} ({}) — projects are not hard-deleted.",
                            archived.name, archived.slug,
                        );
                    }
                },
            )
        }
        ProjectCommand::Move(args) => {
            let product = resolve_product(&mut client, args.product, ctx).await?;
            let project =
                resolve_project(&mut client, &product.id, Some(args.selector), ctx).await?;
            let patch = WorkItemPatch {
                status: Some(args.target.as_str().to_owned()),
                ..WorkItemPatch::default()
            };
            let moved = expect_project(update_work_item(&mut client, &project.id, patch).await?)?;
            print_entity(ctx, &serde_json::json!({ "project": moved }), || {
                print_project_details("Moved project", &moved);
            })
        }
    }
}

async fn run_task_command(command: TaskCommand, ctx: &RunContext) -> Result<(), CliError> {
    let mut client = connect_for_work(ctx).await?;
    match command {
        TaskCommand::Create(args) => {
            let product = resolve_product(&mut client, args.product, ctx).await?;
            let project = resolve_project(&mut client, &product.id, args.project, ctx).await?;
            let name = required_text(args.name, "Task name", ctx)?;
            let description = optional_text(args.description, "Description", ctx)?;
            let task = create_task(
                &mut client,
                CreateTaskInput {
                    product_id: product.id,
                    project_id: project.id,
                    name,
                    description,
                    autostart: !ctx.no_autostart,
                },
            )
            .await?;
            print_entity(ctx, &serde_json::json!({ "task": task }), || {
                print_task_details("Created task", &task);
            })
        }
        TaskCommand::List(args) => {
            let product = resolve_product(&mut client, args.product, ctx).await?;
            let project = match args.project {
                Some(selector) => {
                    Some(resolve_project(&mut client, &product.id, Some(selector), ctx).await?)
                }
                None => None,
            };
            let tasks = list_tasks(
                &mut client,
                &product.id,
                project.as_ref().map(|project| project.id.as_str()),
            )
            .await?;
            print_entity(ctx, &serde_json::json!({ "tasks": tasks }), || {
                print_tasks_table(&tasks)
            })
        }
        TaskCommand::Show(args) => {
            let task = expect_task(get_work_item(&mut client, &args.id).await?)?;
            print_entity(ctx, &serde_json::json!({ "task": task }), || {
                print_task_details("Task", &task);
            })
        }
        TaskCommand::Update(args) => {
            let patch = WorkItemPatch {
                name: args.name,
                description: args.description,
                status: args.status.map(|status| status.as_str().to_owned()),
                ordinal: args.ordinal,
                pr_url: args.pr_url,
                ..WorkItemPatch::default()
            };
            ensure_patch_present(
                &patch,
                "provide at least one field to update, such as --status or --pr-url",
            )?;
            let task = expect_task(update_work_item(&mut client, &args.id, patch).await?)?;
            print_entity(ctx, &serde_json::json!({ "task": task }), || {
                print_task_details("Updated task", &task);
            })
        }
        TaskCommand::Move(args) => {
            let patch = WorkItemPatch {
                status: Some(args.target.as_status().to_owned()),
                ..WorkItemPatch::default()
            };
            let task = expect_task(update_work_item(&mut client, &args.id, patch).await?)?;
            print_entity(ctx, &serde_json::json!({ "task": task }), || {
                print_task_details("Moved task", &task);
            })
        }
        TaskCommand::Delete(args) => {
            delete_work_item(&mut client, &args.id).await?;
            print_entity(
                ctx,
                &serde_json::json!({ "id": args.id, "deleted": true }),
                || {
                    if !ctx.quiet {
                        println!("Deleted task {}", args.id);
                    }
                },
            )
        }
        TaskCommand::Reorder(args) => {
            let product = resolve_product(&mut client, args.product, ctx).await?;
            let project = resolve_project(&mut client, &product.id, args.project, ctx).await?;
            if args.ids.is_empty() {
                return Err(CliError::usage("provide at least one task id via --ids"));
            }
            reorder_project_tasks(&mut client, &project.id, &args.ids).await?;
            print_entity(
                ctx,
                &serde_json::json!({ "project_id": project.id, "task_ids": args.ids }),
                || {
                    if !ctx.quiet {
                        println!(
                            "Reordered {} tasks for project {}",
                            args.ids.len(),
                            project.name
                        );
                    }
                },
            )
        }
    }
}

async fn run_chore_command(command: ChoreCommand, ctx: &RunContext) -> Result<(), CliError> {
    let mut client = connect_for_work(ctx).await?;
    match command {
        ChoreCommand::Create(args) => {
            let product = resolve_product(&mut client, args.product, ctx).await?;
            let name = required_text(args.name, "Chore name", ctx)?;
            let description = optional_text(args.description, "Description", ctx)?;
            let chore = create_chore(
                &mut client,
                CreateChoreInput {
                    product_id: product.id,
                    name,
                    description,
                    autostart: !ctx.no_autostart,
                },
            )
            .await?;
            print_entity(ctx, &serde_json::json!({ "chore": chore }), || {
                print_task_details("Created chore", &chore);
            })
        }
        ChoreCommand::List(args) => {
            let product = resolve_product(&mut client, args.product, ctx).await?;
            let chores = list_chores(&mut client, &product.id).await?;
            let chores = apply_task_list_filters(
                chores,
                &args.status,
                args.match_term.as_deref(),
                &args.id,
                args.limit,
            );
            print_entity(ctx, &serde_json::json!({ "chores": chores }), || {
                print_tasks_table(&chores)
            })
        }
        ChoreCommand::Show(args) => {
            let chore = expect_chore(get_work_item(&mut client, &args.id).await?)?;
            print_entity(ctx, &serde_json::json!({ "chore": chore }), || {
                print_task_details("Chore", &chore);
            })
        }
        ChoreCommand::Update(args) => {
            let patch = WorkItemPatch {
                name: args.name,
                description: args.description,
                status: args.status.map(|status| status.as_str().to_owned()),
                ordinal: args.ordinal,
                pr_url: args.pr_url,
                ..WorkItemPatch::default()
            };
            ensure_patch_present(
                &patch,
                "provide at least one field to update, such as --status or --pr-url",
            )?;
            let chore = expect_chore(update_work_item(&mut client, &args.id, patch).await?)?;
            print_entity(ctx, &serde_json::json!({ "chore": chore }), || {
                print_task_details("Updated chore", &chore);
            })
        }
        ChoreCommand::Move(args) => {
            let patch = WorkItemPatch {
                status: Some(args.target.as_status().to_owned()),
                ..WorkItemPatch::default()
            };
            let chore = expect_chore(update_work_item(&mut client, &args.id, patch).await?)?;
            print_entity(ctx, &serde_json::json!({ "chore": chore }), || {
                print_task_details("Moved chore", &chore);
            })
        }
        ChoreCommand::Delete(args) => {
            delete_work_item(&mut client, &args.id).await?;
            print_entity(
                ctx,
                &serde_json::json!({ "id": args.id, "deleted": true }),
                || {
                    if !ctx.quiet {
                        println!("Deleted chore {}", args.id);
                    }
                },
            )
        }
    }
}

async fn run_engine_command(command: EngineCommand, ctx: &RunContext) -> Result<(), CliError> {
    match command {
        EngineCommand::Status => {
            let running = engine_socket_reachable(&ctx.discovery.socket_path).await;
            let pid = running_engine_pid(&ctx.discovery.pid_file_path);
            print_entity(
                ctx,
                &serde_json::json!({
                    "running": running,
                    "pid": pid,
                    "socket_path": ctx.discovery.socket_path,
                    "pid_file_path": ctx.discovery.pid_file_path,
                }),
                || {
                    if running {
                        println!("Boss engine is running.");
                    } else {
                        println!("Boss engine is stopped.");
                    }
                    println!("Socket: {}", ctx.discovery.socket_path);
                    println!("PID file: {}", ctx.discovery.pid_file_path);
                    if let Some(pid) = pid {
                        println!("PID: {pid}");
                    }
                },
            )
        }
        EngineCommand::Start => {
            ensure_engine_running(&ctx.discovery)
                .await
                .map_err(|err| CliError::engine_unavailable(err.to_string()))?;
            print_entity(
                ctx,
                &serde_json::json!({ "running": true, "socket_path": ctx.discovery.socket_path }),
                || {
                    if !ctx.quiet {
                        println!("Boss engine is running.");
                    }
                },
            )
        }
        EngineCommand::Stop => {
            stop_engine(&ctx.discovery.pid_file_path)
                .map_err(|err| CliError::engine_unavailable(err.to_string()))?;
            print_entity(
                ctx,
                &serde_json::json!({ "running": false, "socket_path": ctx.discovery.socket_path }),
                || {
                    if !ctx.quiet {
                        println!("Stopped Boss engine.");
                    }
                },
            )
        }
    }
}

async fn connect_for_work(ctx: &RunContext) -> Result<BossClient, CliError> {
    BossClient::connect(&ctx.discovery)
        .await
        .map_err(|err| CliError::engine_unavailable(err.to_string()))
}

async fn list_products(client: &mut BossClient) -> Result<Vec<Product>, CliError> {
    match client
        .send_request(&FrontendRequest::ListProducts)
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::ProductsList { products } => Ok(products),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("products list", &other)),
    }
}

async fn list_projects(
    client: &mut BossClient,
    product_id: &str,
) -> Result<Vec<Project>, CliError> {
    match client
        .send_request(&FrontendRequest::ListProjects {
            product_id: product_id.to_owned(),
        })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::ProjectsList { projects, .. } => Ok(projects),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("projects list", &other)),
    }
}

async fn list_tasks(
    client: &mut BossClient,
    product_id: &str,
    project_id: Option<&str>,
) -> Result<Vec<Task>, CliError> {
    match client
        .send_request(&FrontendRequest::ListTasks {
            product_id: product_id.to_owned(),
            project_id: project_id.map(str::to_owned),
        })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::TasksList { tasks, .. } => Ok(tasks),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("tasks list", &other)),
    }
}

async fn list_chores(client: &mut BossClient, product_id: &str) -> Result<Vec<Task>, CliError> {
    match client
        .send_request(&FrontendRequest::ListChores {
            product_id: product_id.to_owned(),
        })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::ChoresList { chores, .. } => Ok(chores),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("chores list", &other)),
    }
}

async fn create_product(
    client: &mut BossClient,
    input: CreateProductInput,
) -> Result<Product, CliError> {
    match client
        .send_request(&FrontendRequest::CreateProduct { input })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::WorkItemCreated { item } => expect_product(item),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("product create", &other)),
    }
}

async fn create_project(
    client: &mut BossClient,
    input: CreateProjectInput,
) -> Result<Project, CliError> {
    match client
        .send_request(&FrontendRequest::CreateProject { input })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::WorkItemCreated { item } => expect_project(item),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("project create", &other)),
    }
}

async fn create_task(client: &mut BossClient, input: CreateTaskInput) -> Result<Task, CliError> {
    match client
        .send_request(&FrontendRequest::CreateTask { input })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::WorkItemCreated { item } => expect_task(item),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("task create", &other)),
    }
}

async fn create_chore(client: &mut BossClient, input: CreateChoreInput) -> Result<Task, CliError> {
    match client
        .send_request(&FrontendRequest::CreateChore { input })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::WorkItemCreated { item } => expect_chore(item),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("chore create", &other)),
    }
}

async fn get_work_item(client: &mut BossClient, id: &str) -> Result<WorkItem, CliError> {
    match client
        .send_request(&FrontendRequest::GetWorkItem { id: id.to_owned() })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::WorkItemResult { item } => Ok(item),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("work item fetch", &other)),
    }
}

async fn update_work_item(
    client: &mut BossClient,
    id: &str,
    patch: WorkItemPatch,
) -> Result<WorkItem, CliError> {
    match client
        .send_request(&FrontendRequest::UpdateWorkItem {
            id: id.to_owned(),
            patch,
        })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::WorkItemUpdated { item } => Ok(item),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("work item update", &other)),
    }
}

async fn delete_work_item(client: &mut BossClient, id: &str) -> Result<(), CliError> {
    match client
        .send_request(&FrontendRequest::DeleteWorkItem { id: id.to_owned() })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::WorkItemDeleted { .. } => Ok(()),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("work item delete", &other)),
    }
}

async fn reorder_project_tasks(
    client: &mut BossClient,
    project_id: &str,
    task_ids: &[String],
) -> Result<(), CliError> {
    match client
        .send_request(&FrontendRequest::ReorderProjectTasks {
            project_id: project_id.to_owned(),
            task_ids: task_ids.to_vec(),
        })
        .await
        .map_err(CliError::internal)?
    {
        FrontendEvent::ProjectTasksReordered { .. } => Ok(()),
        FrontendEvent::WorkError { message } | FrontendEvent::Error { message, .. } => {
            Err(CliError::application(message))
        }
        other => Err(unexpected_event("task reorder", &other)),
    }
}

async fn resolve_product(
    client: &mut BossClient,
    selector: Option<String>,
    ctx: &RunContext,
) -> Result<Product, CliError> {
    let products = list_products(client).await?;
    if products.is_empty() {
        return Err(CliError::not_found("no products exist"));
    }

    let selector = match selector {
        Some(selector) => selector,
        None if products.len() == 1 => return Ok(products[0].clone()),
        None if ctx.allow_input => choose_product(&products)?,
        None => {
            return Err(CliError::usage(
                "product is required; pass --product or run interactively",
            ));
        }
    };

    match_products(&products, &selector)
}

async fn resolve_project(
    client: &mut BossClient,
    product_id: &str,
    selector: Option<String>,
    ctx: &RunContext,
) -> Result<Project, CliError> {
    let projects = list_projects(client, product_id).await?;
    if projects.is_empty() {
        return Err(CliError::not_found(
            "no projects exist for the selected product",
        ));
    }

    let selector = match selector {
        Some(selector) => selector,
        None if projects.len() == 1 => return Ok(projects[0].clone()),
        None if ctx.allow_input => choose_project(&projects)?,
        None => {
            return Err(CliError::usage(
                "project is required; pass --project or run interactively",
            ));
        }
    };

    match_projects(&projects, &selector)
}

fn match_products(products: &[Product], selector: &str) -> Result<Product, CliError> {
    if let Some(product) = pick_by_index(products, selector)? {
        return Ok(product);
    }

    let matches = products
        .iter()
        .filter(|product| product.id == selector || product.slug == selector)
        .cloned()
        .collect::<Vec<_>>();
    resolve_single_match(matches, format!("unknown product: {selector}"))
}

fn match_projects(projects: &[Project], selector: &str) -> Result<Project, CliError> {
    if let Some(project) = pick_by_index(projects, selector)? {
        return Ok(project);
    }

    let matches = projects
        .iter()
        .filter(|project| project.id == selector || project.slug == selector)
        .cloned()
        .collect::<Vec<_>>();
    resolve_single_match(matches, format!("unknown project: {selector}"))
}

fn resolve_single_match<T>(matches: Vec<T>, not_found_message: String) -> Result<T, CliError> {
    match matches.len() {
        0 => Err(CliError::not_found(not_found_message)),
        1 => Ok(matches.into_iter().next().expect("len checked")),
        _ => Err(CliError::conflict(
            "selector resolved to multiple work items",
        )),
    }
}

fn pick_by_index<T: Clone>(items: &[T], selector: &str) -> Result<Option<T>, CliError> {
    let Ok(index) = selector.parse::<usize>() else {
        return Ok(None);
    };
    if !(1..=items.len()).contains(&index) {
        return Err(CliError::usage(format!(
            "selection {index} is out of range; choose a value between 1 and {}",
            items.len()
        )));
    }
    Ok(Some(items[index - 1].clone()))
}

fn choose_product(products: &[Product]) -> Result<String, CliError> {
    println!("Select a product:");
    for (index, product) in products.iter().enumerate() {
        println!("  {}. {} ({})", index + 1, product.name, product.slug);
    }
    prompt_index_or_selector("Product", products.len()).map_err(CliError::internal)
}

fn choose_project(projects: &[Project]) -> Result<String, CliError> {
    println!("Select a project:");
    for (index, project) in projects.iter().enumerate() {
        println!("  {}. {} ({})", index + 1, project.name, project.slug);
    }
    prompt_index_or_selector("Project", projects.len()).map_err(CliError::internal)
}

fn required_text(value: Option<String>, label: &str, ctx: &RunContext) -> Result<String, CliError> {
    if let Some(value) = normalize_non_empty(value) {
        return Ok(value);
    }
    if !ctx.allow_input {
        return Err(CliError::usage(format!(
            "{label} is required; pass it explicitly or omit --no-input"
        )));
    }
    loop {
        let input = prompt_text(label, None).map_err(CliError::internal)?;
        if let Some(value) = normalize_non_empty(Some(input)) {
            return Ok(value);
        }
        eprintln!("{label} cannot be empty.");
    }
}

fn optional_text(
    value: Option<String>,
    label: &str,
    ctx: &RunContext,
) -> Result<Option<String>, CliError> {
    if value.is_some() || !ctx.allow_input {
        return Ok(normalize_non_empty(value));
    }
    let input = prompt_text(label, Some("")).map_err(CliError::internal)?;
    Ok(normalize_non_empty(Some(input)))
}

fn prompt_text(label: &str, default: Option<&str>) -> Result<String> {
    let mut stdout = io::stdout();
    match default {
        Some(default) if !default.is_empty() => write!(stdout, "{label} [{default}]: ")?,
        _ => write!(stdout, "{label}: ")?,
    }
    stdout.flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let input = input.trim_end().to_owned();
    if input.is_empty() {
        Ok(default.unwrap_or_default().to_owned())
    } else {
        Ok(input)
    }
}

fn prompt_index_or_selector(label: &str, count: usize) -> Result<String> {
    loop {
        let input = prompt_text(label, None)?;
        let trimmed = input.trim();
        if trimmed.is_empty() {
            eprintln!("{label} cannot be empty.");
            continue;
        }
        if let Ok(index) = trimmed.parse::<usize>() {
            if (1..=count).contains(&index) {
                return Ok(index.to_string());
            }
            eprintln!("{label} must be between 1 and {count}.");
            continue;
        }
        return Ok(trimmed.to_owned());
    }
}

fn normalize_non_empty(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_owned())
        }
    })
}

fn ensure_patch_present(patch: &WorkItemPatch, message: &str) -> Result<(), CliError> {
    let has_fields = patch.name.is_some()
        || patch.description.is_some()
        || patch.status.is_some()
        || patch.goal.is_some()
        || patch.priority.is_some()
        || patch.repo_remote_url.is_some()
        || patch.pr_url.is_some()
        || patch.ordinal.is_some();

    if has_fields {
        Ok(())
    } else {
        Err(CliError::usage(message))
    }
}

fn expect_product(item: WorkItem) -> Result<Product, CliError> {
    match item {
        WorkItem::Product(product) => Ok(product),
        _ => Err(CliError::conflict("work item is not a product")),
    }
}

fn expect_project(item: WorkItem) -> Result<Project, CliError> {
    match item {
        WorkItem::Project(project) => Ok(project),
        _ => Err(CliError::conflict("work item is not a project")),
    }
}

fn expect_task(item: WorkItem) -> Result<Task, CliError> {
    match item {
        WorkItem::Task(task) => Ok(task),
        WorkItem::Chore(_) => Err(CliError::conflict("work item is a chore, not a task")),
        _ => Err(CliError::conflict("work item is not a task")),
    }
}

fn expect_chore(item: WorkItem) -> Result<Task, CliError> {
    match item {
        WorkItem::Chore(task) => Ok(task),
        WorkItem::Task(_) => Err(CliError::conflict("work item is a task, not a chore")),
        _ => Err(CliError::conflict("work item is not a chore")),
    }
}

fn unexpected_event(context: &str, event: &FrontendEvent) -> CliError {
    CliError::internal(anyhow::anyhow!(
        "unexpected engine event for {context}: {}",
        serde_json::to_string(event).unwrap_or_else(|_| "<unserializable>".to_owned())
    ))
}

fn apply_task_list_filters(
    items: Vec<Task>,
    statuses: &[TaskStatus],
    match_term: Option<&str>,
    ids: &[String],
    limit: Option<usize>,
) -> Vec<Task> {
    let allowed_statuses: Vec<&str> = statuses.iter().map(|s| s.as_str()).collect();
    let id_set: std::collections::HashSet<&str> = ids.iter().map(String::as_str).collect();
    let lc_term = match_term.map(str::to_lowercase);
    items
        .into_iter()
        .filter(|task| {
            if !allowed_statuses.is_empty() && !allowed_statuses.contains(&task.status.as_str()) {
                return false;
            }
            if !id_set.is_empty() && !id_set.contains(task.id.as_str()) {
                return false;
            }
            if let Some(term) = &lc_term {
                let name = task.name.to_lowercase();
                let desc = task.description.to_lowercase();
                if !name.contains(term.as_str()) && !desc.contains(term.as_str()) {
                    return false;
                }
            }
            true
        })
        .take(limit.unwrap_or(usize::MAX))
        .collect()
}

fn apply_project_list_filters(
    items: Vec<Project>,
    statuses: &[ProjectStatus],
    match_term: Option<&str>,
    ids: &[String],
    limit: Option<usize>,
) -> Vec<Project> {
    let allowed_statuses: Vec<&str> = statuses.iter().map(|s| s.as_str()).collect();
    let id_set: std::collections::HashSet<&str> = ids.iter().map(String::as_str).collect();
    let lc_term = match_term.map(str::to_lowercase);
    items
        .into_iter()
        .filter(|project| {
            if !allowed_statuses.is_empty()
                && !allowed_statuses.contains(&project.status.as_str())
            {
                return false;
            }
            if !id_set.is_empty() && !id_set.contains(project.id.as_str()) {
                return false;
            }
            if let Some(term) = &lc_term {
                let name = project.name.to_lowercase();
                let desc = project.description.to_lowercase();
                if !name.contains(term.as_str()) && !desc.contains(term.as_str()) {
                    return false;
                }
            }
            true
        })
        .take(limit.unwrap_or(usize::MAX))
        .collect()
}

fn print_entity<T, F>(ctx: &RunContext, json_value: &T, human: F) -> Result<(), CliError>
where
    T: Serialize,
    F: FnOnce(),
{
    match ctx.output_mode {
        OutputMode::Json => {
            let stdout = io::stdout();
            let mut lock = stdout.lock();
            serde_json::to_writer_pretty(&mut lock, json_value).map_err(CliError::internal)?;
            writeln!(lock).map_err(CliError::internal)?;
        }
        OutputMode::Human => human(),
    }
    Ok(())
}

fn print_products_table(products: &[Product]) {
    let mut table = Table::new();
    table
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(vec!["ID", "SLUG", "NAME", "STATUS", "REPO"]);
    for product in products {
        table.add_row(vec![
            product.id.as_str(),
            product.slug.as_str(),
            product.name.as_str(),
            product.status.as_str(),
            product.repo_remote_url.as_deref().unwrap_or(""),
        ]);
    }
    println!("{table}");
}

fn print_projects_table(projects: &[Project]) {
    let mut table = Table::new();
    table
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(vec!["ID", "SLUG", "NAME", "STATUS", "PRIORITY", "GOAL"]);
    for project in projects {
        table.add_row(vec![
            project.id.as_str(),
            project.slug.as_str(),
            project.name.as_str(),
            project.status.as_str(),
            project.priority.as_str(),
            project.goal.as_str(),
        ]);
    }
    println!("{table}");
}

fn print_tasks_table(tasks: &[Task]) {
    let mut table = Table::new();
    table
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(vec!["ID", "NAME", "STATUS", "PROJECT", "ORDINAL", "PR URL"]);
    for task in tasks {
        let ordinal = task
            .ordinal
            .map(|value| value.to_string())
            .unwrap_or_default();
        table.add_row(vec![
            task.id.as_str(),
            task.name.as_str(),
            task.status.as_str(),
            task.project_id.as_deref().unwrap_or(""),
            ordinal.as_str(),
            task.pr_url.as_deref().unwrap_or(""),
        ]);
    }
    println!("{table}");
}

fn print_product_details(title: &str, product: &Product) {
    println!("{title}");
    println!("ID: {}", product.id);
    println!("Name: {}", product.name);
    println!("Slug: {}", product.slug);
    println!("Status: {}", product.status);
    println!("Repo: {}", product.repo_remote_url.as_deref().unwrap_or(""));
    if !product.description.is_empty() {
        println!("Description: {}", product.description);
    }
}

fn print_project_details(title: &str, project: &Project) {
    println!("{title}");
    println!("ID: {}", project.id);
    println!("Product ID: {}", project.product_id);
    println!("Name: {}", project.name);
    println!("Slug: {}", project.slug);
    println!("Status: {}", project.status);
    println!("Priority: {}", project.priority);
    if !project.goal.is_empty() {
        println!("Goal: {}", project.goal);
    }
    if !project.description.is_empty() {
        println!("Description: {}", project.description);
    }
}

fn print_task_details(title: &str, task: &Task) {
    println!("{title}");
    println!("ID: {}", task.id);
    println!("Product ID: {}", task.product_id);
    if let Some(project_id) = &task.project_id {
        println!("Project ID: {}", project_id);
    }
    println!("Name: {}", task.name);
    println!("Kind: {}", task.kind);
    println!("Status: {}", task.status);
    if let Some(ordinal) = task.ordinal {
        println!("Ordinal: {}", ordinal);
    }
    if let Some(pr_url) = &task.pr_url {
        println!("PR URL: {}", pr_url);
    }
    if !task.description.is_empty() {
        println!("Description: {}", task.description);
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{
        Cli, Commands, MoveTarget, ProductCommand, ProductStatus, ProjectCommand, ProjectStatus,
        TaskCommand, pick_by_index,
    };

    #[test]
    fn move_target_maps_review_to_in_review() {
        assert_eq!(MoveTarget::Review.as_status(), "in_review");
        assert_eq!(MoveTarget::Doing.as_status(), "active");
        assert_eq!(MoveTarget::Blocked.as_status(), "blocked");
    }

    #[test]
    fn parses_product_create_command() {
        let cli = Cli::parse_from(["boss", "product", "create", "--name", "Boss"]);
        match cli.command {
            Commands::Product {
                command: ProductCommand::Create(args),
            } => {
                assert_eq!(args.name.as_deref(), Some("Boss"));
            }
            _ => panic!("expected product create command"),
        }
    }

    #[test]
    fn parses_task_move_command() {
        let cli = Cli::parse_from(["boss", "task", "move", "task_1", "--to", "review"]);
        match cli.command {
            Commands::Task {
                command: TaskCommand::Move(args),
            } => {
                assert_eq!(args.id, "task_1");
                assert!(matches!(args.target, MoveTarget::Review));
            }
            _ => panic!("expected task move command"),
        }
    }

    #[test]
    fn parses_product_delete_command() {
        let cli = Cli::parse_from(["boss", "product", "delete", "boss"]);
        match cli.command {
            Commands::Product {
                command: ProductCommand::Delete(args),
            } => {
                assert_eq!(args.selector, "boss");
            }
            _ => panic!("expected product delete command"),
        }
    }

    #[test]
    fn parses_product_move_command() {
        let cli = Cli::parse_from(["boss", "product", "move", "boss", "--to", "paused"]);
        match cli.command {
            Commands::Product {
                command: ProductCommand::Move(args),
            } => {
                assert_eq!(args.selector, "boss");
                assert!(matches!(args.target, ProductStatus::Paused));
            }
            _ => panic!("expected product move command"),
        }
    }

    #[test]
    fn parses_project_delete_command() {
        let cli = Cli::parse_from([
            "boss", "project", "delete", "work-cli", "--product", "boss",
        ]);
        match cli.command {
            Commands::Project {
                command: ProjectCommand::Delete(args),
            } => {
                assert_eq!(args.selector, "work-cli");
                assert_eq!(args.product.as_deref(), Some("boss"));
            }
            _ => panic!("expected project delete command"),
        }
    }

    #[test]
    fn parses_project_move_command() {
        let cli = Cli::parse_from([
            "boss", "project", "move", "work-cli", "--product", "boss", "--to", "done",
        ]);
        match cli.command {
            Commands::Project {
                command: ProjectCommand::Move(args),
            } => {
                assert_eq!(args.selector, "work-cli");
                assert_eq!(args.product.as_deref(), Some("boss"));
                assert!(matches!(args.target, ProjectStatus::Done));
            }
            _ => panic!("expected project move command"),
        }
    }

    #[test]
    fn product_status_archived_serializes_to_archived() {
        assert_eq!(ProductStatus::Archived.as_str(), "archived");
        assert_eq!(ProductStatus::Active.as_str(), "active");
        assert_eq!(ProductStatus::Paused.as_str(), "paused");
    }

    #[test]
    fn project_status_archived_serializes_to_archived() {
        assert_eq!(ProjectStatus::Archived.as_str(), "archived");
        assert_eq!(ProjectStatus::Done.as_str(), "done");
        assert_eq!(ProjectStatus::Planned.as_str(), "planned");
    }

    #[test]
    fn numeric_selection_is_one_based() {
        let values = vec!["alpha".to_owned(), "beta".to_owned()];
        assert_eq!(
            pick_by_index(&values, "2").unwrap(),
            Some("beta".to_owned())
        );
        assert!(pick_by_index(&values, "0").is_err());
    }
}
