//! `bossctl` — the Boss-only CLI used by the coordinator session
//! running inside the Boss libghostty pane.
//!
//! Two-CLI design (see `tools/boss/docs/designs/main.md`):
//! - `boss` is the user-facing CLI for the work taxonomy
//!   (products / projects / tasks / chores).
//! - `bossctl` is the Boss-only CLI for control verbs
//!   (agents, probe, work start/cancel aliases, workspace summary).
//!
//! Verbs that map cleanly to existing engine RPCs are wired through;
//! verbs that need engine-side surfaces we have not built yet still
//! print a structured "not_implemented" response so the Boss session
//! can call them and see which ones are pending.

use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use boss_client::{BossClient, Discovery};
use boss_protocol::{
    FrontendEvent, FrontendRequest, LiveWorkerState, RequestExecutionInput, ROSTER, WorkExecution,
    WorkRun, WorkspacePoolEntry,
};
use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "bossctl",
    version,
    about = "Boss-only control CLI for the Boss V2 engine",
    long_about = "bossctl drives the Boss V2 engine on behalf of the coordinator session. \
                  Worker sessions do not have access to bossctl — its presence on PATH \
                  is part of how the engine distinguishes Boss-tier requests from worker traffic."
)]
struct Cli {
    /// Override the engine socket path (defaults to `BOSS_SOCKET_PATH`
    /// or the engine's standard path).
    #[arg(long, global = true)]
    socket_path: Option<String>,

    /// Emit machine-readable JSON output where supported.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Inspect and steer worker sessions.
    Agents {
        #[command(subcommand)]
        action: AgentsAction,
    },
    /// Inject a probe prompt that a worker answers on its next Stop
    /// boundary; the reply is observed via the worker's transcript.
    Probe {
        /// Worker reference: run id, slot id, or crew name (e.g.
        /// `Riker`). Crew names resolve only over currently-live
        /// slots; case-insensitive.
        agent: String,
        /// Probe text the worker will see as its next prompt.
        text: String,
    },
    /// Work-item dispatch aliases for symmetry with `boss`.
    Work {
        #[command(subcommand)]
        action: WorkAction,
    },
    /// Inspect the cube workspace pool.
    Workspace {
        #[command(subcommand)]
        action: WorkspaceAction,
    },
}

#[derive(Subcommand, Debug)]
enum AgentsAction {
    /// List worker sessions and their current state.
    List,
    /// Show detailed status for a single worker. Falls back to the
    /// historical run record if the reference is a run id that is no
    /// longer live.
    Status {
        /// Worker reference: run id, slot id, or crew name (e.g.
        /// `Riker`). Crew names resolve only over currently-live
        /// slots; case-insensitive.
        agent: String,
    },
    /// Bring a worker pane to the front.
    Focus {
        /// Worker reference: run id, slot id, or crew name.
        agent: String,
    },
    /// Send text to a worker as if user-typed.
    Send {
        /// Worker reference: run id, slot id, or crew name.
        agent: String,
        text: String,
    },
    /// Interrupt a worker (Esc-equivalent).
    Interrupt {
        /// Worker reference: run id, slot id, or crew name.
        agent: String,
    },
    /// Launch a worker session for a given work item without going
    /// through the coordinator's auto-dispatch path.
    Launch {
        work_item_id: String,
        #[arg(long)]
        preferred_workspace_id: Option<String>,
    },
    /// Stop a worker session and release its lease.
    Stop {
        /// Worker reference: run id, slot id, or crew name.
        agent: String,
    },
    /// Print the most recent transcript chunk from a worker.
    Transcript {
        /// Worker reference: run id, slot id, or crew name.
        agent: String,
        #[arg(long, default_value_t = 100)]
        lines: usize,
    },
}

#[derive(Subcommand, Debug)]
enum WorkAction {
    /// Request the engine schedule a work item for execution.
    Start {
        work_item_id: String,
        #[arg(long)]
        priority: Option<i64>,
        #[arg(long)]
        preferred_workspace_id: Option<String>,
    },
    /// Cancel a queued or running execution.
    Cancel { execution_id: String },
}

#[derive(Subcommand, Debug)]
enum WorkspaceAction {
    /// Summarize cube workspace pool state.
    Summary,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("bossctl: failed to start tokio runtime: {err}");
            return ExitCode::from(1);
        }
    };
    match runtime.block_on(dispatch(cli)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("bossctl: {err:#}");
            ExitCode::from(1)
        }
    }
}

async fn dispatch(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Probe { agent, text } => {
            probe_run(&cli.socket_path, cli.json, agent, text).await
        }
        Command::Agents {
            action: AgentsAction::Status { agent },
        } => agents_status(&cli.socket_path, cli.json, agent).await,
        Command::Agents {
            action: AgentsAction::List,
        } => agents_list_live(&cli.socket_path, cli.json).await,
        Command::Agents {
            action: AgentsAction::Stop { agent },
        } => agents_stop(&cli.socket_path, cli.json, agent).await,
        Command::Agents {
            action: AgentsAction::Focus { agent },
        } => agents_focus(&cli.socket_path, cli.json, agent).await,
        Command::Agents {
            action: AgentsAction::Transcript { agent, lines },
        } => agents_transcript(&cli.socket_path, cli.json, agent, lines).await,
        Command::Work {
            action:
                WorkAction::Start {
                    work_item_id,
                    priority,
                    preferred_workspace_id,
                },
        } => work_start(
            &cli.socket_path,
            cli.json,
            work_item_id,
            priority,
            preferred_workspace_id,
        )
        .await,
        Command::Work {
            action: WorkAction::Cancel { execution_id },
        } => work_cancel(&cli.socket_path, cli.json, execution_id).await,
        Command::Workspace {
            action: WorkspaceAction::Summary,
        } => workspace_summary(&cli.socket_path, cli.json).await,
        // The remaining verbs need engine surfaces that don't exist
        // yet (interrupt key injection, focus pane, send keystrokes,
        // explicit launch). They print a structured "not_implemented"
        // so the Boss session can call them and see exactly which
        // ones are pending.
        other => print_not_implemented(cli.json, &describe_verb(&other)),
    }
}

async fn connect(socket_path: &Option<String>) -> Result<BossClient> {
    let discovery = Discovery::from_env(socket_path.as_deref())
        .context("resolving engine discovery profile")?;
    BossClient::connect(&discovery)
        .await
        .context("connecting to engine")
}

/// Resolve a positional `agent` argument to a live worker entry.
///
/// Tries, in order: (a) exact match on `run_id`, (b) exact match on
/// numeric `slot_id`, (c) case-insensitive exact match on crew
/// `name`. The first non-empty tier wins; an ambiguous tier (more
/// than one match) errors with the candidate list.
///
/// Names resolve only over currently-live slots — historical run
/// ids stay run-id-only on purpose, so a typo'd crew name doesn't
/// silently match a closed run.
fn resolve_agent_ref<'a>(
    reference: &str,
    states: &'a [LiveWorkerState],
) -> Result<&'a LiveWorkerState> {
    let by_run: Vec<&LiveWorkerState> =
        states.iter().filter(|s| s.run_id == reference).collect();
    if !by_run.is_empty() {
        return pick_unique(reference, by_run, states);
    }
    if let Ok(slot) = reference.parse::<u8>() {
        let by_slot: Vec<&LiveWorkerState> =
            states.iter().filter(|s| s.slot_id == slot).collect();
        if !by_slot.is_empty() {
            return pick_unique(reference, by_slot, states);
        }
    }
    let by_name: Vec<&LiveWorkerState> = states
        .iter()
        .filter(|s| s.name.eq_ignore_ascii_case(reference))
        .collect();
    if !by_name.is_empty() {
        return pick_unique(reference, by_name, states);
    }
    bail!(
        "no live worker matches `{reference}`. {}",
        live_candidates_summary(states),
    )
}

fn pick_unique<'a>(
    reference: &str,
    matches: Vec<&'a LiveWorkerState>,
    states: &'a [LiveWorkerState],
) -> Result<&'a LiveWorkerState> {
    if matches.len() == 1 {
        return Ok(matches[0]);
    }
    bail!(
        "`{reference}` matches multiple live workers: {}. {}",
        matches
            .iter()
            .map(|s| format!("slot {} ({}) run {}", s.slot_id, s.name, s.run_id))
            .collect::<Vec<_>>()
            .join(", "),
        live_candidates_summary(states),
    )
}

fn live_candidates_summary(states: &[LiveWorkerState]) -> String {
    if states.is_empty() {
        return "no live workers".into();
    }
    let mut sorted: Vec<&LiveWorkerState> = states.iter().collect();
    sorted.sort_by_key(|s| s.slot_id);
    let labels: Vec<String> = sorted
        .iter()
        .map(|s| format!("slot {} ({})", s.slot_id, s.name))
        .collect();
    format!("Live: {}", labels.join(", "))
}

/// True if `reference` looks like a name or numeric slot id (so a
/// resolver miss should be terminal rather than falling back to a
/// historical run-id lookup). A run id like `exec_18ad...` falls
/// through both checks.
fn looks_like_name_or_slot(reference: &str) -> bool {
    if reference.parse::<u8>().is_ok() {
        return true;
    }
    ROSTER
        .iter()
        .any(|name| name.eq_ignore_ascii_case(reference))
}

async fn fetch_live_states(client: &mut BossClient) -> Result<Vec<LiveWorkerState>> {
    match client
        .send_request(&FrontendRequest::ListWorkerLiveStates)
        .await
        .context("sending ListWorkerLiveStates")?
    {
        FrontendEvent::WorkerLiveStatesList { states } => Ok(states),
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("engine rejected list: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    }
}

async fn probe_run(
    socket_path: &Option<String>,
    json: bool,
    agent: String,
    text: String,
) -> Result<()> {
    let mut client = connect(socket_path).await?;
    let states = fetch_live_states(&mut client).await?;
    let run_id = resolve_agent_ref(&agent, &states)?.run_id.clone();
    let response = client
        .send_request(&FrontendRequest::ProbeRun {
            run_id: run_id.clone(),
            text,
        })
        .await
        .context("sending ProbeRun")?;
    match response {
        FrontendEvent::ProbeQueued { run_id: returned } => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "status": "queued",
                        "run_id": returned,
                    })
                );
            } else {
                println!("probe queued for run {returned}");
            }
            Ok(())
        }
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("engine rejected probe: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    }
}

/// Show live runtime status for the worker referenced by `agent`
/// (run id, slot id, or crew name). Falls back to the finalised
/// `WorkRun` record (the historical snapshot the engine persists
/// once the run row finalises) when the reference looks like a
/// run id and no matching live entry is found — so the verb still
/// works for runs that have already terminated. Crew-name and
/// slot-id references that miss are *not* fall through to the
/// historical lookup; they error with the live candidate list to
/// avoid silently matching a typo against a closed run.
async fn agents_status(socket_path: &Option<String>, json: bool, agent: String) -> Result<()> {
    let mut client = connect(socket_path).await?;
    let states = fetch_live_states(&mut client).await?;

    match resolve_agent_ref(&agent, &states) {
        Ok(state) => {
            print_live_state(json, state);
            return Ok(());
        }
        Err(err) if looks_like_name_or_slot(&agent) => return Err(err),
        Err(_) => {}
    }

    // No live entry and the reference doesn't look like a name or
    // slot — assume it's a historical run id.
    let response = client
        .send_request(&FrontendRequest::GetRun { id: agent.clone() })
        .await
        .context("sending GetRun")?;
    match response {
        FrontendEvent::RunResult { run } => {
            print_run(json, &run);
            Ok(())
        }
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("engine rejected status: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    }
}

/// List every live worker slot (model, activity, current tool, last
/// event time). Unlike the previous `agents list`, this is sourced
/// from the engine's in-memory LiveWorkerState rather than from the
/// finalised WorkRun records — those finalise within ~1s of spawn
/// and don't reflect the live worker.
async fn agents_list_live(socket_path: &Option<String>, json: bool) -> Result<()> {
    let mut client = connect(socket_path).await?;
    let states = fetch_live_states(&mut client).await?;

    if json {
        println!(
            "{}",
            serde_json::json!({
                "live_worker_states": states,
            })
        );
    } else if states.is_empty() {
        println!("no active workers");
    } else {
        for state in &states {
            print_live_state_short(state);
        }
    }
    Ok(())
}

async fn agents_stop(socket_path: &Option<String>, json: bool, agent: String) -> Result<()> {
    let mut client = connect(socket_path).await?;
    let states = fetch_live_states(&mut client).await?;
    let run_id = resolve_agent_ref(&agent, &states)?.run_id.clone();
    let response = client
        .send_request(&FrontendRequest::StopRun {
            run_id: run_id.clone(),
        })
        .await
        .context("sending StopRun")?;
    match response {
        FrontendEvent::RunStopped { run_id: returned } => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "status": "stopped",
                        "run_id": returned,
                    })
                );
            } else {
                println!("stopped run {returned}");
            }
            Ok(())
        }
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("engine rejected stop: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    }
}

async fn agents_focus(socket_path: &Option<String>, json: bool, agent: String) -> Result<()> {
    let mut client = connect(socket_path).await?;
    let states = fetch_live_states(&mut client).await?;
    let run_id = resolve_agent_ref(&agent, &states)?.run_id.clone();
    let response = client
        .send_request(&FrontendRequest::FocusWorkerPane {
            run_id: run_id.clone(),
        })
        .await
        .context("sending FocusWorkerPane")?;
    match response {
        FrontendEvent::WorkerPaneFocused {
            run_id: returned,
            slot_id,
        } => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "status": "focused",
                        "run_id": returned,
                        "slot_id": slot_id,
                    })
                );
            } else {
                println!("focused slot {slot_id} (run {returned})");
            }
            Ok(())
        }
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("engine rejected focus: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    }
}

async fn work_start(
    socket_path: &Option<String>,
    json: bool,
    work_item_id: String,
    priority: Option<i64>,
    preferred_workspace_id: Option<String>,
) -> Result<()> {
    let mut client = connect(socket_path).await?;
    let response = client
        .send_request(&FrontendRequest::RequestExecution {
            input: RequestExecutionInput {
                work_item_id: work_item_id.clone(),
                priority,
                preferred_workspace_id,
            },
        })
        .await
        .context("sending RequestExecution")?;
    match response {
        FrontendEvent::ExecutionRequested { execution }
        | FrontendEvent::ExecutionCreated { execution }
        | FrontendEvent::ExecutionResult { execution } => {
            print_execution(json, &execution);
            Ok(())
        }
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("engine rejected work start: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    }
}

async fn work_cancel(
    socket_path: &Option<String>,
    json: bool,
    execution_id: String,
) -> Result<()> {
    let mut client = connect(socket_path).await?;
    let response = client
        .send_request(&FrontendRequest::CancelExecution {
            execution_id: execution_id.clone(),
        })
        .await
        .context("sending CancelExecution")?;
    match response {
        FrontendEvent::ExecutionCancelled { execution } => {
            if json {
                println!(
                    "{}",
                    serde_json::to_string(&execution).expect("WorkExecution serializes")
                );
            } else {
                println!("cancelled execution {}", execution.id);
                println!("  status:    {}", execution.status);
                if let Some(f) = &execution.finished_at {
                    println!("  finished:  {f}");
                }
            }
            Ok(())
        }
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("engine rejected work cancel: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    }
}

async fn agents_transcript(
    socket_path: &Option<String>,
    json: bool,
    agent: String,
    lines: usize,
) -> Result<()> {
    let mut client = connect(socket_path).await?;
    let states = fetch_live_states(&mut client).await?;
    let run_id = resolve_agent_ref(&agent, &states)?.run_id.clone();
    let response = client
        .send_request(&FrontendRequest::TailRunTranscript {
            run_id: run_id.clone(),
            lines,
        })
        .await
        .context("sending TailRunTranscript")?;
    match response {
        FrontendEvent::RunTranscriptTail {
            run_id: returned,
            transcript_path,
            lines: tail,
            truncated,
        } => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "run_id": returned,
                        "transcript_path": transcript_path,
                        "lines": tail,
                        "truncated": truncated,
                    })
                );
            } else {
                if truncated {
                    println!(
                        "transcript {transcript_path} (showing last {} lines; older content omitted)",
                        tail.len()
                    );
                } else {
                    println!("transcript {transcript_path} ({} lines)", tail.len());
                }
                for line in tail {
                    println!("{line}");
                }
            }
            Ok(())
        }
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("engine rejected transcript tail: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    }
}

async fn workspace_summary(socket_path: &Option<String>, json: bool) -> Result<()> {
    let mut client = connect(socket_path).await?;
    let response = client
        .send_request(&FrontendRequest::WorkspacePoolSummary)
        .await
        .context("sending WorkspacePoolSummary")?;
    match response {
        FrontendEvent::WorkspacePoolSummaryResult { workspaces } => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "workspaces": workspaces,
                    })
                );
            } else if workspaces.is_empty() {
                println!("no workspaces in cube pool");
            } else {
                for ws in &workspaces {
                    print_workspace_entry_short(ws);
                }
            }
            Ok(())
        }
        FrontendEvent::Error { message, .. } | FrontendEvent::WorkError { message } => {
            bail!("engine rejected workspace summary: {message}")
        }
        other => bail!("engine returned unexpected response: {other:?}"),
    }
}

fn print_workspace_entry_short(entry: &WorkspacePoolEntry) {
    let lease = entry.lease_id.as_deref().unwrap_or("-");
    let exec = entry.execution_id.as_deref().unwrap_or("-");
    let task = entry.task.as_deref().unwrap_or("-");
    println!(
        "{}  state={}  lease={}  execution={}  task=\"{}\"  path={}",
        entry.workspace_id, entry.state, lease, exec, task, entry.workspace_path,
    );
}

fn print_run(json: bool, run: &WorkRun) {
    if json {
        println!("{}", serde_json::to_string(run).expect("WorkRun serializes"));
    } else {
        println!("run {}", run.id);
        println!("  execution:  {}", run.execution_id);
        println!("  agent:      {}", run.agent_id);
        println!("  status:     {}", run.status);
        if let Some(s) = &run.started_at {
            println!("  started:    {s}");
        }
        if let Some(f) = &run.finished_at {
            println!("  finished:   {f}");
        }
        if let Some(t) = &run.transcript_path {
            println!("  transcript: {t}");
        }
        if let Some(err) = &run.error_text {
            println!("  error:      {err}");
        }
    }
}

#[allow(dead_code)]
fn print_run_short(run: &WorkRun) {
    let started = run.started_at.as_deref().unwrap_or("-");
    println!(
        "{}  agent={}  {}  {}  exec={}",
        run.id, run.agent_id, run.status, started, run.execution_id
    );
}

fn print_live_state(json: bool, state: &LiveWorkerState) {
    if json {
        println!(
            "{}",
            serde_json::to_string(state).expect("LiveWorkerState serializes")
        );
        return;
    }
    println!("slot {} ({})", state.slot_id, state.name);
    println!("  run:           {}", state.run_id);
    println!("  model:         {}", state.model);
    println!("  activity:      {}", state.activity.as_str());
    println!("  shell_pid:     {}", state.shell_pid);
    if let Some(id) = &state.work_item_id {
        println!("  work_item:     {id}");
    }
    if let Some(name) = &state.work_item_name {
        println!("  work_item_name:{name}");
    }
    if let Some(id) = &state.execution_id {
        println!("  execution:     {id}");
    }
    if let Some(tool) = &state.current_tool {
        println!("  current_tool:  {tool}");
    }
    if let Some(ts) = &state.last_event_at {
        println!("  last_event_at: {ts}");
    }
    if let Some(ts) = &state.last_tool_ended_at {
        println!("  last_tool_end: {ts}");
    }
}

fn print_live_state_short(state: &LiveWorkerState) {
    let tool = state.current_tool.as_deref().unwrap_or("-");
    let work_item = state.work_item_id.as_deref().unwrap_or("-");
    let work_item_name = state.work_item_name.as_deref().unwrap_or("-");
    println!(
        "slot {}  name={}  run={}  model={}  activity={}  tool={}  work_item={}  work_item_name=\"{}\"",
        state.slot_id,
        state.name,
        state.run_id,
        state.model,
        state.activity.as_str(),
        tool,
        work_item,
        work_item_name,
    );
}

fn print_execution(json: bool, execution: &WorkExecution) {
    if json {
        println!(
            "{}",
            serde_json::to_string(execution).expect("WorkExecution serializes")
        );
    } else {
        println!("execution {}", execution.id);
        println!("  work_item: {}", execution.work_item_id);
        println!("  kind:      {}", execution.kind);
        println!("  status:    {}", execution.status);
        if let Some(p) = &execution.workspace_path {
            println!("  workspace: {p}");
        }
    }
}

fn print_not_implemented(json: bool, verb: &str) -> Result<()> {
    if json {
        println!(
            "{}",
            serde_json::json!({
                "status": "not_implemented",
                "verb": verb,
            })
        );
    } else {
        println!("bossctl {verb}: not yet implemented");
    }
    Ok(())
}

fn describe_verb(command: &Command) -> String {
    match command {
        Command::Agents { action } => match action {
            AgentsAction::List => "agents list".into(),
            AgentsAction::Status { .. } => "agents status".into(),
            AgentsAction::Focus { .. } => "agents focus".into(),
            AgentsAction::Send { .. } => "agents send".into(),
            AgentsAction::Interrupt { .. } => "agents interrupt".into(),
            AgentsAction::Launch { .. } => "agents launch".into(),
            AgentsAction::Stop { .. } => "agents stop".into(),
            AgentsAction::Transcript { .. } => "agents transcript".into(),
        },
        Command::Probe { .. } => "probe".into(),
        Command::Work { action } => match action {
            WorkAction::Start { .. } => "work start".into(),
            WorkAction::Cancel { .. } => "work cancel".into(),
        },
        Command::Workspace { action } => match action {
            WorkspaceAction::Summary => "workspace summary".into(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use boss_protocol::WorkerActivity;

    fn live(slot: u8, run: &str) -> LiveWorkerState {
        LiveWorkerState {
            slot_id: slot,
            name: boss_protocol::name_for_slot(slot),
            run_id: run.into(),
            model: "claude-opus-4-7".into(),
            shell_pid: 0,
            last_event_at: None,
            current_tool: None,
            last_tool_ended_at: None,
            activity: WorkerActivity::Idle,
            work_item_id: None,
            work_item_name: None,
            execution_id: None,
        }
    }

    #[test]
    fn resolves_by_run_id() {
        let states = vec![live(1, "exec_a"), live(2, "exec_b")];
        let hit = resolve_agent_ref("exec_b", &states).unwrap();
        assert_eq!(hit.slot_id, 2);
    }

    #[test]
    fn resolves_by_numeric_slot_id() {
        let states = vec![live(1, "exec_a"), live(3, "exec_c")];
        let hit = resolve_agent_ref("3", &states).unwrap();
        assert_eq!(hit.run_id, "exec_c");
    }

    #[test]
    fn resolves_by_crew_name_case_insensitive() {
        let states = vec![live(1, "exec_a"), live(2, "exec_b")];
        // slot 1 = Riker, slot 2 = Data
        let hit = resolve_agent_ref("riker", &states).unwrap();
        assert_eq!(hit.slot_id, 1);
        let hit = resolve_agent_ref("DATA", &states).unwrap();
        assert_eq!(hit.slot_id, 2);
    }

    #[test]
    fn resolves_la_forge_with_space() {
        // Slot 4 is "La Forge" — the space matters for exact match.
        let states = vec![live(4, "exec_d")];
        let hit = resolve_agent_ref("la forge", &states).unwrap();
        assert_eq!(hit.slot_id, 4);
    }

    #[test]
    fn unknown_reference_lists_live_candidates() {
        let states = vec![live(1, "exec_a"), live(2, "exec_b")];
        let err = resolve_agent_ref("Wesley", &states).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("no live worker matches"), "msg: {msg}");
        assert!(msg.contains("Riker"), "msg: {msg}");
        assert!(msg.contains("Data"), "msg: {msg}");
    }

    #[test]
    fn unknown_with_no_live_workers_says_so() {
        let states: Vec<LiveWorkerState> = vec![];
        let err = resolve_agent_ref("Riker", &states).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("no live workers"), "msg: {msg}");
    }

    #[test]
    fn run_id_takes_precedence_over_slot_match() {
        // If a run_id happens to be the literal "1", the run_id tier
        // wins before slot_id is even consulted (a defensive case
        // since real run ids are not numeric strings).
        let mut s1 = live(2, "1");
        // Force a different slot so a slot match would resolve to a
        // different worker; run_id "1" should still win.
        s1.slot_id = 2;
        let states = vec![s1, live(1, "exec_a")];
        let hit = resolve_agent_ref("1", &states).unwrap();
        assert_eq!(hit.run_id, "1");
        assert_eq!(hit.slot_id, 2);
    }

    #[test]
    fn looks_like_name_or_slot_recognises_roster_and_numbers() {
        assert!(looks_like_name_or_slot("Riker"));
        assert!(looks_like_name_or_slot("riker"));
        assert!(looks_like_name_or_slot("La Forge"));
        assert!(looks_like_name_or_slot("3"));
        assert!(!looks_like_name_or_slot("exec_18ad6336fedcb190_12"));
    }
}
