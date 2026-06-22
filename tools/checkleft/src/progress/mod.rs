//! Interactive progress/status UI for `checkleft run`.
//!
//! This module is presentation-only: it never changes which checks run, what
//! they find, or the final exit code. It is also perfectly suppressible — the
//! [`NoopProgressReporter`] used on the non-interactive path performs no I/O at
//! all, so disabled output is byte-identical to a build without this module.
//!
//! Three pieces:
//! - [`ProgressReporter`]: the thread-safe sink the runner emits lifecycle
//!   events to. The runner holds an `Arc<dyn ProgressReporter>` and clones it
//!   into each concurrent check task.
//! - [`state::ProgressState`]: the renderer-agnostic state machine (transitions,
//!   anti-flicker debounce, pass/fail counting, timing).
//! - [`LiveProgress`]: the interactive driver — owns the shared state plus a
//!   background thread that redraws on a timer for the spinner animation.
//!
//! See [`render`] for the terminal layout (scrolling log above, pinned per-check
//! status below).

pub mod render;
pub mod state;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::output::{CheckResult, Severity};

use self::render::{Renderer, draw_frame};
use self::state::ProgressState;

/// How long a check must run before it is promoted to a spinner line. Checks
/// that finish faster settle straight into their result line (anti-flicker).
pub const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(120);

/// Spinner animation / redraw cadence.
const TICK: Duration = Duration::from_millis(80);

/// Renders a check's findings to the "normal rendered form" shown in the log
/// area. Supplied by the binary, which owns the human formatter.
pub type RenderFindings = Arc<dyn Fn(&CheckResult) -> String + Send + Sync>;

/// Thread-safe sink for per-check progress lifecycle events.
///
/// Checks run concurrently, so every method takes `&self` and implementations
/// must be internally synchronized. All methods are presentation-only.
pub trait ProgressReporter: Send + Sync {
    /// A check is about to run against `total_files` files. Seeds its status line
    /// in a stable position; the line stays hidden until the check starts and
    /// crosses the debounce window (or finishes).
    fn register(&self, check_id: &str, total_files: usize);

    /// The check has begun executing (starts its elapsed clock + debounce).
    fn start(&self, check_id: &str);

    /// Optional per-file progress: `processed` of the check's files handled so
    /// far. The built-in runner does not currently emit this (checks are opaque
    /// futures), but the channel exists for checks that can report granular
    /// progress.
    fn record_progress(&self, check_id: &str, processed: usize);

    /// The check finished. `files_failed` is the number of files with
    /// error/warning findings (0 ⇒ clean). Updates the status line only; stream
    /// the findings themselves with [`Self::stream_findings`].
    fn finish(&self, check_id: &str, files_failed: usize, elapsed: Duration);

    /// Stream a result's findings into the scrolling log area in their normal
    /// rendered form. A no-op when the result has no findings.
    fn stream_findings(&self, result: &CheckResult);
}

/// Number of distinct files with error/warning findings in a result — the
/// "NN files failed" count. Falls back to the count of error/warning findings
/// without a location (e.g. check-level failures) so a failing check never
/// renders as passed.
pub fn files_failed_count(result: &CheckResult) -> usize {
    use std::collections::HashSet;
    let mut paths: HashSet<&std::path::Path> = HashSet::new();
    let mut locationless = 0usize;
    for finding in &result.findings {
        if !matches!(finding.severity, Severity::Error | Severity::Warning) {
            continue;
        }
        match &finding.location {
            Some(location) => {
                paths.insert(location.path.as_path());
            }
            None => locationless += 1,
        }
    }
    paths.len() + locationless
}

/// A progress reporter that does nothing. Used on the non-interactive /
/// disabled path so output is byte-identical to a build without progress.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopProgressReporter;

impl ProgressReporter for NoopProgressReporter {
    fn register(&self, _check_id: &str, _total_files: usize) {}
    fn start(&self, _check_id: &str) {}
    fn record_progress(&self, _check_id: &str, _processed: usize) {}
    fn finish(&self, _check_id: &str, _files_failed: usize, _elapsed: Duration) {}
    fn stream_findings(&self, _result: &CheckResult) {}
}

/// Split a rendered finding block into individual lines, dropping the single
/// trailing newline so the log area does not gain a stray blank line per check.
fn split_block(text: &str) -> Vec<String> {
    let mut lines: Vec<&str> = text.split('\n').collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    lines.into_iter().map(str::to_owned).collect()
}

/// The live reporter handed to the runner: mutates shared state and renders
/// findings to text via the binary-supplied formatter.
struct LiveReporter {
    shared: Arc<Mutex<ProgressState>>,
    render_findings: RenderFindings,
}

impl ProgressReporter for LiveReporter {
    fn register(&self, check_id: &str, total_files: usize) {
        self.shared.lock().unwrap().register(check_id, total_files);
    }

    fn start(&self, check_id: &str) {
        self.shared.lock().unwrap().start(check_id, Instant::now());
    }

    fn record_progress(&self, check_id: &str, processed: usize) {
        self.shared.lock().unwrap().record_progress(check_id, processed);
    }

    fn finish(&self, check_id: &str, files_failed: usize, elapsed: Duration) {
        self.shared.lock().unwrap().finish(check_id, files_failed, elapsed);
    }

    fn stream_findings(&self, result: &CheckResult) {
        if result.findings.is_empty() {
            return;
        }
        let rendered = (self.render_findings)(result);
        let lines = split_block(&rendered);
        if !lines.is_empty() {
            self.shared.lock().unwrap().push_log_lines(lines);
        }
    }
}

/// Interactive progress driver. Owns the shared state and a background thread
/// that redraws the status block on a timer (for the spinner animation). Drop or
/// [`Self::finalize`] stops the thread and leaves the final frame on screen.
pub struct LiveProgress {
    shared: Arc<Mutex<ProgressState>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl LiveProgress {
    /// Start the render loop with `renderer` and the given debounce window.
    pub fn new(renderer: Box<dyn Renderer>, debounce: Duration) -> Self {
        let shared = Arc::new(Mutex::new(ProgressState::new(debounce)));
        let stop = Arc::new(AtomicBool::new(false));
        let handle = spawn_render_loop(renderer, Arc::clone(&shared), Arc::clone(&stop));
        Self {
            shared,
            stop,
            handle: Some(handle),
        }
    }

    /// A reporter sharing this driver's state. `render_findings` formats a
    /// result's findings into the rendered form streamed into the log area.
    pub fn reporter(&self, render_findings: RenderFindings) -> Arc<dyn ProgressReporter> {
        Arc::new(LiveReporter {
            shared: Arc::clone(&self.shared),
            render_findings,
        })
    }

    /// Stop the render loop and draw the final frame. Idempotent. Call once all
    /// checks have finished (so every reporter call has already landed).
    pub fn finalize(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for LiveProgress {
    fn drop(&mut self) {
        self.finalize();
    }
}

fn spawn_render_loop(
    mut renderer: Box<dyn Renderer>,
    shared: Arc<Mutex<ProgressState>>,
    stop: Arc<AtomicBool>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut tick = 0usize;
        loop {
            // Observe the stop flag *before* drawing so the final draw flushes any
            // log lines and final status pushed just before finalize().
            let stopping = stop.load(Ordering::SeqCst);
            {
                let mut state = shared.lock().unwrap();
                let _ = draw_frame(renderer.as_mut(), &mut state, Instant::now(), tick);
            }
            if stopping {
                let _ = renderer.finalize();
                break;
            }
            tick = tick.wrapping_add(1);
            std::thread::sleep(TICK);
        }
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::output::{Finding, Location, Severity};

    use super::*;

    fn finding(severity: Severity, path: Option<&str>) -> Finding {
        Finding {
            severity,
            message: "m".to_owned(),
            location: path.map(|p| Location {
                path: PathBuf::from(p),
                line: None,
                column: None,
            }),
            remediations: vec![],
            suggested_fix: None,
        }
    }

    #[test]
    fn files_failed_counts_distinct_error_warning_paths() {
        let result = CheckResult {
            check_id: "c".to_owned(),
            findings: vec![
                finding(Severity::Error, Some("a.rs")),
                finding(Severity::Warning, Some("a.rs")), // same file, deduped
                finding(Severity::Error, Some("b.rs")),
                finding(Severity::Info, Some("c.rs")), // info ignored
            ],
        };
        assert_eq!(files_failed_count(&result), 2);
    }

    #[test]
    fn files_failed_counts_locationless_failures() {
        let result = CheckResult {
            check_id: "c".to_owned(),
            findings: vec![finding(Severity::Error, None)],
        };
        assert_eq!(files_failed_count(&result), 1);
    }

    #[test]
    fn files_failed_is_zero_for_info_only() {
        let result = CheckResult {
            check_id: "c".to_owned(),
            findings: vec![finding(Severity::Info, Some("a.rs"))],
        };
        assert_eq!(files_failed_count(&result), 0);
    }

    #[test]
    fn split_block_drops_single_trailing_newline() {
        assert_eq!(split_block("a\nb\n"), vec!["a".to_owned(), "b".to_owned()]);
        // An internal blank line (finding separator) is preserved.
        assert_eq!(
            split_block("a\n\nb\n"),
            vec!["a".to_owned(), "".to_owned(), "b".to_owned()]
        );
    }

    #[test]
    fn noop_reporter_is_inert() {
        let reporter = NoopProgressReporter;
        reporter.register("c", 3);
        reporter.start("c");
        reporter.record_progress("c", 1);
        reporter.finish("c", 0, Duration::from_millis(1));
        reporter.stream_findings(&CheckResult {
            check_id: "c".to_owned(),
            findings: vec![finding(Severity::Error, Some("a.rs"))],
        });
        // Nothing to assert beyond "does not panic / does no I/O".
    }

    #[test]
    fn live_reporter_streams_into_shared_state() {
        use super::render::testing::RecordingRenderer;

        let renderer = Box::new(RecordingRenderer::default());
        let mut live = LiveProgress::new(renderer, DEFAULT_DEBOUNCE);
        let reporter = live.reporter(Arc::new(|result: &CheckResult| format!("rendered:{}", result.check_id)));

        reporter.register("typo", 2);
        reporter.start("typo");
        reporter.stream_findings(&CheckResult {
            check_id: "typo".to_owned(),
            findings: vec![finding(Severity::Error, Some("a.rs"))],
        });
        reporter.finish("typo", 1, Duration::from_millis(5));

        live.finalize();
        // After finalize the shared state is terminal and the log was consumed by
        // the render loop (drain), so nothing remains buffered.
        let mut state = live.shared.lock().unwrap();
        assert!(state.all_done());
        assert!(state.drain_log().is_empty());
    }
}
