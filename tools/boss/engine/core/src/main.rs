use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use clap::Parser;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use boss_engine::app;
use boss_engine::audit::{self, StartContext};
use boss_engine::build_info;
use boss_engine::cli::Cli;
use boss_engine::trace_rotation::{self, RotatingJsonlWriter, RotatingState};

const DEFAULT_LOG_PATH: &str = "/tmp/boss-engine.log";

struct DualLogWriter {
    stderr: io::Stderr,
    file: Option<Arc<Mutex<File>>>,
}

impl DualLogWriter {
    fn new(file: Option<Arc<Mutex<File>>>) -> Self {
        Self {
            stderr: io::stderr(),
            file,
        }
    }
}

impl Write for DualLogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.stderr.write_all(buf)?;
        if let Some(file) = &self.file {
            if let Ok(mut file) = file.lock() {
                let _ = file.write_all(buf);
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stderr.flush()?;
        if let Some(file) = &self.file {
            if let Ok(mut file) = file.lock() {
                let _ = file.flush();
            }
        }
        Ok(())
    }
}


/// Path for the structured-JSON engine trace file consumed by the Activity
/// Log viewer in the macOS app. Lives alongside other Boss state files.
fn engine_trace_jsonl_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(
        PathBuf::from(home)
            .join("Library/Application Support/Boss")
            .join("engine-trace.jsonl"),
    )
}

fn resolve_log_path() -> PathBuf {
    std::env::var("BOSS_ENGINE_LOG_PATH")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_LOG_PATH))
}

fn open_log_file(path: &Path) -> Result<File> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create log directory {}", parent.display()))?;
        }
    }

    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("failed to open engine log file {}", path.display()))
}

#[tokio::main]
async fn main() -> Result<()> {
    // Handle --version before the full startup so we print our custom
    // "boss-engine 0+<sha> built <time>" format and exit cleanly
    // without initialising logging or touching the audit log.
    // Q-Risk-3 from the design doc: this flag did not exist before this change.
    let argv: Vec<String> = std::env::args().collect();
    if argv.get(1).map(|s| s.as_str()) == Some("--version")
        || argv.get(1).map(|s| s.as_str()) == Some("-V")
    {
        println!("{}", build_info::version_string("boss-engine"));
        return Ok(());
    }

    let log_path = resolve_log_path();
    let file_writer = match open_log_file(&log_path) {
        Ok(file) => Some(Arc::new(Mutex::new(file))),
        Err(err) => {
            eprintln!(
                "boss-engine: could not enable file logging at {}: {err}",
                log_path.display()
            );
            None
        }
    };

    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    // Text layer: compact human-readable output to stderr + rolling log file.
    let text_layer = tracing_subscriber::fmt::layer()
        .compact()
        .with_target(false)
        .with_writer(move || DualLogWriter::new(file_writer.clone()));

    // JSON layer: structured JSONL for the macOS Activity Log viewer.
    // Best-effort — silently skipped if the file cannot be opened.
    // On startup the existing trace file (if any) is rotated to a
    // timestamped backup; a size-based rotation fires mid-run when the
    // threshold is crossed.
    let (trace_max_bytes, trace_max_files) = trace_rotation::trace_rotation_config();
    let (json_trace_path, json_state_arc) = match engine_trace_jsonl_path() {
        None => (PathBuf::new(), Arc::new(Mutex::new(None))),
        Some(path) => {
            trace_rotation::rotate_on_startup(&path, trace_max_files);
            let state = match trace_rotation::open_trace_file(&path) {
                Ok(file) => Some(RotatingState::new(file)),
                Err(err) => {
                    eprintln!(
                        "boss-engine: could not open engine-trace JSONL at {}: {err}",
                        path.display()
                    );
                    None
                }
            };
            (path, Arc::new(Mutex::new(state)))
        }
    };
    let json_layer = tracing_subscriber::fmt::layer()
        .json()
        .with_writer(move || RotatingJsonlWriter {
            path: json_trace_path.clone(),
            state: json_state_arc.clone(),
            max_bytes: trace_max_bytes,
            max_files: trace_max_files,
        });

    tracing_subscriber::registry()
        .with(env_filter)
        .with(text_layer)
        .with(json_layer)
        .init();

    tracing::info!(log_path = %log_path.display(), "boss-engine logging initialized");

    if let Some(audit_path) = audit::default_audit_log_path() {
        audit::set_audit_path(audit_path);
    }

    audit::record_start(build_start_context());

    install_audit_panic_hook();

    let cli = Cli::parse();
    let result = app::run(cli).await;

    let reason = match &result {
        Ok(()) => "normal".to_owned(),
        Err(err) => format!("error:{}", short_error(err)),
    };
    audit::record_shutdown(reason);

    result
}

fn build_start_context() -> StartContext {
    let argv: Vec<String> = std::env::args().collect();
    let parent_command = parent_command_line();
    let engine_version = std::env::var("BOSS_ENGINE_VERSION")
        .ok()
        .or_else(|| option_env!("CARGO_PKG_VERSION").map(|s| s.to_owned()));
    let state_db_path = std::env::var_os("BOSS_DB_PATH")
        .map(PathBuf::from)
        .or_else(default_state_db_path);
    let prior_state_db_size = state_db_path
        .as_ref()
        .and_then(|p| std::fs::metadata(p).ok())
        .map(|m| m.len());
    let socket_paths = collect_known_socket_paths();
    StartContext {
        argv,
        engine_version,
        socket_paths,
        state_db_path,
        prior_state_db_size,
        parent_command,
    }
}

/// Best-effort lookup of the parent process command line. Uses `ps`
/// because nothing else in the engine pulls in a procfs / sysctl
/// dependency and `ps` is reliably available on macOS.
fn parent_command_line() -> Option<String> {
    let ppid = unsafe { libc::getppid() };
    if ppid <= 0 {
        return None;
    }
    let output = std::process::Command::new("ps")
        .args(["-o", "command=", "-p", &ppid.to_string()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if line.is_empty() { None } else { Some(line) }
}

fn default_state_db_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join("Library/Application Support/Boss/state.db"))
}

fn collect_known_socket_paths() -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = Vec::new();
    // Frontend socket: BOSS_SOCKET_PATH overrides the default; the
    // engine itself reads `cli.socket_path`, but the env mirrors the
    // CLI's default and is what the macOS app sets.
    if let Some(p) = std::env::var_os("BOSS_SOCKET_PATH") {
        paths.push(PathBuf::from(p));
    } else {
        paths.push(PathBuf::from("/tmp/boss-engine.sock"));
    }

    if let Some(p) = std::env::var_os("BOSS_EVENTS_SOCKET") {
        paths.push(PathBuf::from(p));
    } else if let Some(home) = std::env::var_os("HOME") {
        paths.push(PathBuf::from(home).join("Library/Application Support/Boss/events.sock"));
    }

    paths
}

/// Wrap the existing panic hook so a crash record always lands in the
/// audit log before the process unwinds. We do this in `main` rather
/// than inside `app::serve`'s panic hook so even a panic during init
/// (config load, log dir creation, etc.) leaves a trail.
fn install_audit_panic_hook() {
    let prior = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let payload = info
            .payload()
            .downcast_ref::<&'static str>()
            .copied()
            .or_else(|| info.payload().downcast_ref::<String>().map(|s| s.as_str()))
            .unwrap_or("panic");
        let reason = format!("crash:{}", short_message(payload));
        audit::record_shutdown(reason);
        prior(info);
    }));
}

fn short_error(err: &anyhow::Error) -> String {
    short_message(&format!("{err}"))
}

fn short_message(msg: &str) -> String {
    let trimmed: String = msg.lines().next().unwrap_or("").chars().take(200).collect();
    if trimmed.is_empty() {
        "unknown".to_owned()
    } else {
        trimmed
    }
}
