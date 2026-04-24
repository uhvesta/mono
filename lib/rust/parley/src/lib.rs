use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use thiserror::Error;

const DEFAULT_CAPTURE_SCROLLBACK_LINES: usize = 260;
const DEFAULT_TAIL_LINES: usize = 24;
const DEFAULT_SEND_CHUNK_BYTES: usize = 900;
const DEFAULT_SEND_INTER_CHUNK_DELAY_MS: u64 = 30;
const DEFAULT_WAIT_POLL_MS: u64 = 250;
const DEFAULT_WAIT_TIMEOUT_SECS: u64 = 120;
const DEFAULT_IDLE_DEBOUNCE_POLLS: u32 = 2;
const BUSY_MARKER: &str = "esc to interrupt";
const PROMPT_MARKER: &str = "❯";
const SUBMIT_KEY: &str = "C-m";

#[derive(Debug, Clone)]
pub struct TmuxController {
    tmux_program: OsString,
    capture_scrollback_lines: usize,
    tail_lines: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchOptions {
    pub session_name: String,
    pub window_name: Option<String>,
    pub command: ClaudeLaunchCommand,
    pub cwd: Option<PathBuf>,
    pub env: BTreeMap<String, String>,
    pub unset_env: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaudeLaunchCommand {
    PlainClaude,
    Custom(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneHandle {
    pub session_name: String,
    pub window_index: u32,
    pub pane_index: u32,
}

#[derive(Debug, Clone)]
pub struct ClaudePane {
    controller: TmuxController,
    pane: PaneHandle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneSnapshot {
    pub captured_at: SystemTime,
    pub text: String,
    pub tail: String,
    pub busy: bool,
    pub prompt_visible: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneState {
    Starting,
    Idle,
    Busy,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendOptions {
    pub chunk_bytes: usize,
    pub inter_chunk_delay: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitOptions {
    pub poll_interval: Duration,
    pub idle_debounce_polls: u32,
    pub timeout: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnResult {
    pub completed_at: SystemTime,
    pub duration: Duration,
    pub final_snapshot: PaneSnapshot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnPhase {
    WaitingForIdleBeforeSend,
    WaitingForBusyAfterSubmit,
    WaitingForIdleAfterSubmit,
}

#[derive(Debug, Error)]
pub enum ControllerError {
    #[error("tmux command failed: {command} (status: {status:?}): {stderr}")]
    TmuxCommandFailed {
        command: String,
        status: Option<i32>,
        stderr: String,
    },
    #[error("session not found: {0}")]
    SessionNotFound(String),
    #[error("pane not found: {0}")]
    PaneNotFound(String),
    #[error("timed out during {phase:?} after {timeout:?}")]
    TimedOut { phase: TurnPhase, timeout: Duration },
    #[error("chunk size must be greater than zero")]
    InvalidChunkSize,
    #[error("launch command cannot be empty")]
    EmptyLaunchCommand,
    #[error("unexpected pane output: {0}")]
    UnexpectedPaneOutput(String),
}

impl Default for SendOptions {
    fn default() -> Self {
        Self {
            chunk_bytes: DEFAULT_SEND_CHUNK_BYTES,
            inter_chunk_delay: Duration::from_millis(DEFAULT_SEND_INTER_CHUNK_DELAY_MS),
        }
    }
}

impl Default for WaitOptions {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_millis(DEFAULT_WAIT_POLL_MS),
            idle_debounce_polls: DEFAULT_IDLE_DEBOUNCE_POLLS,
            timeout: Duration::from_secs(DEFAULT_WAIT_TIMEOUT_SECS),
        }
    }
}

impl LaunchOptions {
    pub fn plain_claude(session_name: impl Into<String>) -> Self {
        let mut unset_env = BTreeSet::new();
        unset_env.insert("NO_COLOR".to_owned());

        Self {
            session_name: session_name.into(),
            window_name: None,
            command: ClaudeLaunchCommand::PlainClaude,
            cwd: None,
            env: BTreeMap::new(),
            unset_env,
        }
    }
}

impl PaneHandle {
    pub fn target(&self) -> String {
        format!(
            "{}:{}.{}",
            self.session_name, self.window_index, self.pane_index
        )
    }
}

impl ClaudePane {
    pub fn new(controller: TmuxController, pane: PaneHandle) -> Self {
        Self { controller, pane }
    }

    pub fn pane(&self) -> &PaneHandle {
        &self.pane
    }

    pub fn snapshot(&self) -> Result<PaneSnapshot, ControllerError> {
        self.controller.capture(&self.pane)
    }

    pub fn wait_until_idle(&self, options: &WaitOptions) -> Result<TurnResult, ControllerError> {
        self.controller.wait_for_idle(&self.pane, options)
    }

    pub fn say(&self, prompt: &str) -> Result<TurnResult, ControllerError> {
        self.controller.run_turn(
            &self.pane,
            prompt,
            &SendOptions::default(),
            &WaitOptions::default(),
        )
    }
}

impl Default for TmuxController {
    fn default() -> Self {
        Self::new()
    }
}

impl TmuxController {
    pub fn new() -> Self {
        Self {
            tmux_program: OsString::from("tmux"),
            capture_scrollback_lines: DEFAULT_CAPTURE_SCROLLBACK_LINES,
            tail_lines: DEFAULT_TAIL_LINES,
        }
    }

    pub fn with_tmux_program(tmux_program: impl Into<OsString>) -> Self {
        Self {
            tmux_program: tmux_program.into(),
            ..Self::new()
        }
    }

    pub fn launch_session(&self, options: &LaunchOptions) -> Result<PaneHandle, ControllerError> {
        let mut args = vec![
            OsString::from("new-session"),
            OsString::from("-d"),
            OsString::from("-s"),
            OsString::from(options.session_name.clone()),
        ];

        if let Some(window_name) = &options.window_name {
            args.push(OsString::from("-n"));
            args.push(OsString::from(window_name));
        }

        if let Some(cwd) = &options.cwd {
            args.push(OsString::from("-c"));
            args.push(cwd.as_os_str().to_owned());
        }

        args.push(OsString::from(build_launch_command(options)?));

        self.run_tmux(&args)?;
        self.find_pane(&options.session_name)
    }

    pub fn find_pane(&self, session_name: &str) -> Result<PaneHandle, ControllerError> {
        let output = self.run_tmux(&[
            OsString::from("list-panes"),
            OsString::from("-t"),
            OsString::from(session_name),
            OsString::from("-F"),
            OsString::from("#{window_index} #{pane_index}"),
        ])?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let first_line = stdout
            .lines()
            .find(|line| !line.trim().is_empty())
            .ok_or_else(|| ControllerError::PaneNotFound(session_name.to_owned()))?;
        let mut parts = first_line.split_whitespace();
        let window_index = parts
            .next()
            .ok_or_else(|| ControllerError::UnexpectedPaneOutput(first_line.to_owned()))?
            .parse::<u32>()
            .map_err(|_| ControllerError::UnexpectedPaneOutput(first_line.to_owned()))?;
        let pane_index = parts
            .next()
            .ok_or_else(|| ControllerError::UnexpectedPaneOutput(first_line.to_owned()))?
            .parse::<u32>()
            .map_err(|_| ControllerError::UnexpectedPaneOutput(first_line.to_owned()))?;

        Ok(PaneHandle {
            session_name: session_name.to_owned(),
            window_index,
            pane_index,
        })
    }

    pub fn capture(&self, pane: &PaneHandle) -> Result<PaneSnapshot, ControllerError> {
        let output = self.run_tmux(&[
            OsString::from("capture-pane"),
            OsString::from("-p"),
            OsString::from("-t"),
            OsString::from(pane.target()),
            OsString::from("-S"),
            OsString::from(format!("-{}", self.capture_scrollback_lines)),
        ])?;
        let text = String::from_utf8_lossy(&output.stdout).into_owned();
        Ok(PaneSnapshot {
            captured_at: SystemTime::now(),
            tail: extract_tail(&text, self.tail_lines),
            busy: is_busy(&text),
            prompt_visible: prompt_visible(&text),
            text,
        })
    }

    pub fn state(&self, pane: &PaneHandle) -> Result<PaneState, ControllerError> {
        let snapshot = self.capture(pane)?;
        Ok(classify_state(&snapshot))
    }

    pub fn pane(&self, pane: PaneHandle) -> ClaudePane {
        ClaudePane::new(self.clone(), pane)
    }

    pub fn send_text(
        &self,
        pane: &PaneHandle,
        text: &str,
        options: &SendOptions,
    ) -> Result<(), ControllerError> {
        let chunks = chunk_text(text, options.chunk_bytes)?;
        for chunk in chunks {
            self.run_tmux(&[
                OsString::from("send-keys"),
                OsString::from("-t"),
                OsString::from(pane.target()),
                OsString::from("-l"),
                OsString::from("--"),
                OsString::from(chunk),
            ])?;
            if !options.inter_chunk_delay.is_zero() {
                thread::sleep(options.inter_chunk_delay);
            }
        }
        Ok(())
    }

    pub fn submit(&self, pane: &PaneHandle) -> Result<(), ControllerError> {
        self.run_tmux(&[
            OsString::from("send-keys"),
            OsString::from("-t"),
            OsString::from(pane.target()),
            OsString::from(SUBMIT_KEY),
        ])?;
        Ok(())
    }

    pub fn wait_for_idle(
        &self,
        pane: &PaneHandle,
        options: &WaitOptions,
    ) -> Result<TurnResult, ControllerError> {
        let mut tracker = WaitTracker::for_idle(options.idle_debounce_polls);
        let start = Instant::now();

        loop {
            let snapshot = self.capture(pane)?;
            if tracker.observe(&snapshot).done {
                return Ok(TurnResult {
                    completed_at: snapshot.captured_at,
                    duration: start.elapsed(),
                    final_snapshot: snapshot,
                });
            }
            if start.elapsed() >= options.timeout {
                return Err(ControllerError::TimedOut {
                    phase: TurnPhase::WaitingForIdleBeforeSend,
                    timeout: options.timeout,
                });
            }
            thread::sleep(options.poll_interval);
        }
    }

    pub fn run_turn(
        &self,
        pane: &PaneHandle,
        prompt: &str,
        send: &SendOptions,
        wait: &WaitOptions,
    ) -> Result<TurnResult, ControllerError> {
        self.wait_for_idle(pane, wait)?;
        let baseline = self.capture(pane)?;
        self.send_text(pane, prompt, send)?;
        self.submit(pane)?;
        self.wait_for_turn_completion(pane, &baseline, wait)
    }

    fn wait_for_turn_completion(
        &self,
        pane: &PaneHandle,
        baseline: &PaneSnapshot,
        options: &WaitOptions,
    ) -> Result<TurnResult, ControllerError> {
        let mut tracker = WaitTracker::for_turn(&baseline.tail, options.idle_debounce_polls);
        let start = Instant::now();

        loop {
            let snapshot = self.capture(pane)?;
            let observation = tracker.observe(&snapshot);
            if observation.done {
                return Ok(TurnResult {
                    completed_at: snapshot.captured_at,
                    duration: start.elapsed(),
                    final_snapshot: snapshot,
                });
            }
            if start.elapsed() >= options.timeout {
                let phase = if tracker.progress_seen() {
                    TurnPhase::WaitingForIdleAfterSubmit
                } else {
                    TurnPhase::WaitingForBusyAfterSubmit
                };
                return Err(ControllerError::TimedOut {
                    phase,
                    timeout: options.timeout,
                });
            }
            thread::sleep(options.poll_interval);
        }
    }

    fn run_tmux(&self, args: &[OsString]) -> Result<std::process::Output, ControllerError> {
        let output = Command::new(&self.tmux_program)
            .args(args)
            .output()
            .map_err(|err| ControllerError::TmuxCommandFailed {
                command: format_command(&self.tmux_program, args),
                status: None,
                stderr: err.to_string(),
            })?;

        if output.status.success() {
            Ok(output)
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            let stderr = if stderr.is_empty() {
                String::from_utf8_lossy(&output.stdout).trim().to_owned()
            } else {
                stderr
            };
            let error = if stderr.contains("can't find session") {
                ControllerError::SessionNotFound(stderr)
            } else {
                ControllerError::TmuxCommandFailed {
                    command: format_command(&self.tmux_program, args),
                    status: output.status.code(),
                    stderr,
                }
            };
            Err(error)
        }
    }
}

#[derive(Debug)]
struct WaitObservation {
    done: bool,
}

#[derive(Debug, Clone)]
enum WaitMode {
    Idle,
    Turn { baseline_tail: String },
}

#[derive(Debug, Clone)]
struct WaitTracker {
    mode: WaitMode,
    idle_debounce_polls: u32,
    last_tail: Option<String>,
    stable_idle_polls: u32,
    progress_seen: bool,
}

impl WaitTracker {
    fn for_idle(idle_debounce_polls: u32) -> Self {
        Self {
            mode: WaitMode::Idle,
            idle_debounce_polls: idle_debounce_polls.max(1),
            last_tail: None,
            stable_idle_polls: 0,
            progress_seen: true,
        }
    }

    fn for_turn(baseline_tail: &str, idle_debounce_polls: u32) -> Self {
        Self {
            mode: WaitMode::Turn {
                baseline_tail: baseline_tail.to_owned(),
            },
            idle_debounce_polls: idle_debounce_polls.max(1),
            last_tail: None,
            stable_idle_polls: 0,
            progress_seen: false,
        }
    }

    fn progress_seen(&self) -> bool {
        self.progress_seen
    }

    fn observe(&mut self, snapshot: &PaneSnapshot) -> WaitObservation {
        match &self.mode {
            WaitMode::Idle => {}
            WaitMode::Turn { baseline_tail } => {
                if snapshot.busy || snapshot.tail != *baseline_tail {
                    self.progress_seen = true;
                }
            }
        }

        let stable = self
            .last_tail
            .as_deref()
            .is_some_and(|last| last == snapshot.tail);

        if snapshot.busy {
            self.stable_idle_polls = 0;
        } else if stable {
            self.stable_idle_polls += 1;
        } else {
            self.stable_idle_polls = 1;
        }

        self.last_tail = Some(snapshot.tail.clone());

        WaitObservation {
            done: !snapshot.busy
                && self.progress_seen
                && self.stable_idle_polls >= self.idle_debounce_polls,
        }
    }
}

fn build_launch_command(options: &LaunchOptions) -> Result<String, ControllerError> {
    let command = match &options.command {
        ClaudeLaunchCommand::PlainClaude => vec!["claude".to_owned()],
        ClaudeLaunchCommand::Custom(parts) => {
            if parts.is_empty() {
                return Err(ControllerError::EmptyLaunchCommand);
            }
            parts.clone()
        }
    };

    let mut tokens = Vec::new();
    if !options.unset_env.is_empty() || !options.env.is_empty() {
        tokens.push("env".to_owned());
        for key in &options.unset_env {
            tokens.push("-u".to_owned());
            tokens.push(key.clone());
        }
        for (key, value) in &options.env {
            tokens.push(format!("{key}={value}"));
        }
    }
    tokens.extend(command);

    Ok(tokens
        .iter()
        .map(|token| shell_escape(token))
        .collect::<Vec<_>>()
        .join(" "))
}

fn format_command(program: &OsString, args: &[OsString]) -> String {
    let mut rendered = Vec::with_capacity(args.len() + 1);
    rendered.push(program.to_string_lossy().into_owned());
    rendered.extend(args.iter().map(|arg| arg.to_string_lossy().into_owned()));
    rendered.join(" ")
}

fn shell_escape(token: &str) -> String {
    if token.is_empty() {
        return "''".to_owned();
    }
    format!("'{}'", token.replace('\'', "'\\''"))
}

fn extract_tail(text: &str, tail_lines: usize) -> String {
    let mut lines = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();
    let keep = tail_lines.min(lines.len());
    lines.drain(..lines.len().saturating_sub(keep));
    lines.join("\n")
}

fn is_busy(text: &str) -> bool {
    text.contains(BUSY_MARKER)
}

fn prompt_visible(text: &str) -> bool {
    text.lines()
        .any(|line| line.trim_start().starts_with(PROMPT_MARKER))
}

fn classify_state(snapshot: &PaneSnapshot) -> PaneState {
    if snapshot.busy {
        PaneState::Busy
    } else if snapshot.prompt_visible {
        PaneState::Idle
    } else if snapshot.text.contains("Accessing workspace:")
        || snapshot.text.contains("Quick safety check:")
    {
        PaneState::Starting
    } else {
        PaneState::Unknown
    }
}

fn chunk_text(text: &str, chunk_bytes: usize) -> Result<Vec<&str>, ControllerError> {
    if chunk_bytes == 0 {
        return Err(ControllerError::InvalidChunkSize);
    }

    let mut chunks = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let mut end = (start + chunk_bytes).min(text.len());
        while !text.is_char_boundary(end) {
            end = end.saturating_sub(1);
        }
        if end == start {
            return Err(ControllerError::InvalidChunkSize);
        }
        chunks.push(&text[start..end]);
        start = end;
    }
    Ok(chunks)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(text: &str) -> PaneSnapshot {
        PaneSnapshot {
            captured_at: SystemTime::UNIX_EPOCH,
            tail: extract_tail(text, DEFAULT_TAIL_LINES),
            busy: is_busy(text),
            prompt_visible: prompt_visible(text),
            text: text.to_owned(),
        }
    }

    #[test]
    fn chunk_text_splits_on_char_boundaries() {
        let chunks = chunk_text("hello🙂world", 7).expect("chunk text");
        assert_eq!(chunks, vec!["hello", "🙂wor", "ld"]);
    }

    #[test]
    fn chunk_text_rejects_zero_chunk_size() {
        let error = chunk_text("hello", 0).expect_err("chunking should fail");
        assert!(matches!(error, ControllerError::InvalidChunkSize));
    }

    #[test]
    fn extract_tail_discards_blank_lines() {
        let tail = extract_tail("one\n\n\ntwo\n\nthree\n", 2);
        assert_eq!(tail, "two\nthree");
    }

    #[test]
    fn classify_state_reports_busy_from_footer_marker() {
        let snap = snapshot("reply\n  ⏵⏵ auto mode on (shift+tab to cycle) · esc to interrupt\n");
        assert_eq!(classify_state(&snap), PaneState::Busy);
    }

    #[test]
    fn classify_state_reports_idle_when_prompt_is_visible() {
        let snap = snapshot("hello\n❯ \n  ⏵⏵ auto mode on (shift+tab to cycle)\n");
        assert_eq!(classify_state(&snap), PaneState::Idle);
    }

    #[test]
    fn wait_for_idle_requires_stable_idle_polls() {
        let mut tracker = WaitTracker::for_idle(2);
        let first = tracker.observe(&snapshot("hello\n❯ \n"));
        assert!(!first.done);
        let second = tracker.observe(&snapshot("hello\n❯ \n"));
        assert!(second.done);
    }

    #[test]
    fn wait_for_turn_completes_after_busy_then_stable_idle() {
        let baseline = snapshot("❯ \n");
        let mut tracker = WaitTracker::for_turn(&baseline.tail, 2);
        let busy = tracker.observe(&snapshot(
            "❯ testing\n  ⏵⏵ auto mode on (shift+tab to cycle) · esc to interrupt\n",
        ));
        assert!(!busy.done);
        let idle1 = tracker.observe(&snapshot("❯ testing\n⏺ ok\n❯ \n"));
        assert!(!idle1.done);
        let idle2 = tracker.observe(&snapshot("❯ testing\n⏺ ok\n❯ \n"));
        assert!(idle2.done);
    }

    #[test]
    fn wait_for_turn_completes_for_fast_idle_response_without_busy() {
        let baseline = snapshot("❯ \n");
        let mut tracker = WaitTracker::for_turn(&baseline.tail, 2);
        let first = tracker.observe(&snapshot("❯ hi\n⏺ hello\n❯ \n"));
        assert!(!first.done);
        let second = tracker.observe(&snapshot("❯ hi\n⏺ hello\n❯ \n"));
        assert!(second.done);
    }

    #[test]
    fn build_launch_command_unsets_no_color_and_quotes_args() {
        let options = LaunchOptions {
            session_name: "cc".to_owned(),
            window_name: None,
            command: ClaudeLaunchCommand::Custom(vec![
                "claude".to_owned(),
                "--prompt".to_owned(),
                "say 'hello'".to_owned(),
            ]),
            cwd: None,
            env: BTreeMap::from([("COLORTERM".to_owned(), "truecolor".to_owned())]),
            unset_env: BTreeSet::from(["NO_COLOR".to_owned()]),
        };

        let command = build_launch_command(&options).expect("build command");
        assert_eq!(
            command,
            "'env' '-u' 'NO_COLOR' 'COLORTERM=truecolor' 'claude' '--prompt' 'say '\\''hello'\\'''"
        );
    }
}
