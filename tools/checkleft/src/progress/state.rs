//! The progress state machine.
//!
//! [`ProgressState`] holds one entry per executing check and turns lifecycle
//! events (register → start → finish, plus optional file-progress increments)
//! into the visible status lines the renderer draws. It is deliberately free of
//! any terminal or I/O concern so the whole anti-flicker / counting / timing
//! contract is unit-testable without a real terminal (see the tests below).
//!
//! Timing is injected: every method that cares about "now" takes an [`Instant`]
//! argument rather than reading the clock, so tests drive elapsed time
//! deterministically with `base + Duration`.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Which glyph the renderer should draw for a status line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconKind {
    /// Animated spinner — the check has been running past the debounce window.
    Spinner,
    /// Green check — the check finished with no error/warning findings.
    Passed,
    /// Red x — the check finished with at least one error/warning finding.
    Failed,
}

/// One rendered status line: the semantic icon plus its text. The renderer maps
/// [`IconKind`] (+ the animation tick) to a concrete colored glyph; keeping the
/// glyph out of here is what lets the state machine be asserted in tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusLine {
    pub icon: IconKind,
    pub text: String,
}

#[derive(Debug, Clone, Copy)]
enum Lifecycle {
    Pending,
    Running { started_at: Instant },
    Done { files_failed: usize, elapsed: Duration },
}

#[derive(Debug)]
struct Entry {
    id: String,
    total_files: usize,
    processed_files: usize,
    lifecycle: Lifecycle,
}

/// The shared, renderer-agnostic progress model.
#[derive(Debug)]
pub struct ProgressState {
    entries: Vec<Entry>,
    index: HashMap<String, usize>,
    /// A check is only promoted to the spinner line after it has been running
    /// for at least this long; faster checks settle straight into their result
    /// line (anti-flicker).
    debounce: Duration,
    /// Rendered finding lines awaiting a flush into the scrolling log area.
    log: Vec<String>,
}

impl ProgressState {
    pub fn new(debounce: Duration) -> Self {
        Self {
            entries: Vec::new(),
            index: HashMap::new(),
            debounce,
            log: Vec::new(),
        }
    }

    fn entry_mut(&mut self, check_id: &str) -> &mut Entry {
        if let Some(&i) = self.index.get(check_id) {
            return &mut self.entries[i];
        }
        let i = self.entries.len();
        self.entries.push(Entry {
            id: check_id.to_owned(),
            total_files: 0,
            processed_files: 0,
            lifecycle: Lifecycle::Pending,
        });
        self.index.insert(check_id.to_owned(), i);
        &mut self.entries[i]
    }

    /// Register a check that is about to run, seeding its line in stable order.
    /// Pending entries are not yet visible (see [`Self::visible_lines`]).
    pub fn register(&mut self, check_id: &str, total_files: usize) {
        let entry = self.entry_mut(check_id);
        entry.total_files = total_files;
    }

    /// Mark a check as started, beginning its elapsed clock and debounce window.
    pub fn start(&mut self, check_id: &str, now: Instant) {
        self.entry_mut(check_id).lifecycle = Lifecycle::Running { started_at: now };
    }

    /// Record that `processed` of the check's files have been handled so far.
    /// Drives the in-progress `k/N files checked` counter. Saturates at the
    /// known total.
    pub fn record_progress(&mut self, check_id: &str, processed: usize) {
        let entry = self.entry_mut(check_id);
        entry.processed_files = if entry.total_files > 0 {
            processed.min(entry.total_files)
        } else {
            processed
        };
    }

    /// Mark a check finished. `files_failed` is the count of files that produced
    /// error/warning findings (0 ⇒ the check passed cleanly).
    pub fn finish(&mut self, check_id: &str, files_failed: usize, elapsed: Duration) {
        self.entry_mut(check_id).lifecycle = Lifecycle::Done { files_failed, elapsed };
    }

    /// Queue already-rendered finding lines to be flushed above the status block.
    pub fn push_log_lines(&mut self, lines: impl IntoIterator<Item = String>) {
        self.log.extend(lines);
    }

    /// Take the pending log lines, leaving the buffer empty.
    pub fn drain_log(&mut self) -> Vec<String> {
        std::mem::take(&mut self.log)
    }

    /// True once every registered check has reached a terminal state.
    pub fn all_done(&self) -> bool {
        self.entries
            .iter()
            .all(|e| matches!(e.lifecycle, Lifecycle::Done { .. }))
    }

    /// Compute the status lines to draw at `now`, in stable registration order.
    ///
    /// Anti-flicker: a `Pending` check, or a `Running` check that has not yet
    /// crossed the debounce window, is omitted entirely — so a check that
    /// finishes faster than the debounce never flashes a spinner; its first and
    /// only appearance is its final result line.
    pub fn visible_lines(&self, now: Instant) -> Vec<StatusLine> {
        let mut lines = Vec::new();
        for entry in &self.entries {
            match entry.lifecycle {
                Lifecycle::Pending => {}
                Lifecycle::Running { started_at } => {
                    let elapsed = now.saturating_duration_since(started_at);
                    if elapsed < self.debounce {
                        continue; // still inside the debounce window — stay hidden
                    }
                    lines.push(StatusLine {
                        icon: IconKind::Spinner,
                        text: in_progress_text(entry, elapsed),
                    });
                }
                Lifecycle::Done { files_failed, elapsed } => {
                    let (icon, text) = if files_failed > 0 {
                        (IconKind::Failed, done_failed_text(entry, files_failed, elapsed))
                    } else {
                        (IconKind::Passed, done_passed_text(entry, elapsed))
                    };
                    lines.push(StatusLine { icon, text });
                }
            }
        }
        lines
    }
}

fn in_progress_text(entry: &Entry, elapsed: Duration) -> String {
    let timing = format_elapsed(elapsed);
    if entry.processed_files > 0 {
        format!(
            "{}: {}/{} files checked [{timing}]",
            entry.id, entry.processed_files, entry.total_files
        )
    } else {
        format!(
            "{}: checking {} [{timing}]",
            entry.id,
            pluralize_files(entry.total_files)
        )
    }
}

fn done_failed_text(entry: &Entry, files_failed: usize, elapsed: Duration) -> String {
    format!(
        "{}: {} failed [{}]",
        entry.id,
        pluralize_files(files_failed),
        format_elapsed(elapsed)
    )
}

fn done_passed_text(entry: &Entry, elapsed: Duration) -> String {
    format!(
        "{}: {} passed [{}]",
        entry.id,
        pluralize_files(entry.total_files),
        format_elapsed(elapsed)
    )
}

fn pluralize_files(n: usize) -> String {
    if n == 1 {
        "1 file".to_owned()
    } else {
        format!("{n} files")
    }
}

/// Render an elapsed duration the way the status lines show it: `0ms`, `123ms`,
/// `1s`, `1.5s`, `12s`.
pub fn format_elapsed(elapsed: Duration) -> String {
    let ms = elapsed.as_millis();
    if ms >= 1000 {
        let secs = ms as f64 / 1000.0;
        if (secs.fract()).abs() < f64::EPSILON {
            format!("{}s", secs as u64)
        } else {
            format!("{secs:.1}s")
        }
    } else {
        format!("{ms}ms")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEBOUNCE: Duration = Duration::from_millis(120);

    fn text_of(lines: &[StatusLine]) -> Vec<&str> {
        lines.iter().map(|l| l.text.as_str()).collect()
    }

    #[test]
    fn format_elapsed_matches_documented_examples() {
        assert_eq!(format_elapsed(Duration::from_millis(0)), "0ms");
        assert_eq!(format_elapsed(Duration::from_millis(123)), "123ms");
        assert_eq!(format_elapsed(Duration::from_millis(1000)), "1s");
        assert_eq!(format_elapsed(Duration::from_millis(1500)), "1.5s");
        assert_eq!(format_elapsed(Duration::from_millis(12000)), "12s");
    }

    #[test]
    fn pending_and_unregistered_checks_are_hidden() {
        let mut state = ProgressState::new(DEBOUNCE);
        state.register("typo", 3);
        let now = Instant::now();
        assert!(state.visible_lines(now).is_empty(), "pending check must not render");
    }

    #[test]
    fn fast_check_never_shows_a_spinner() {
        // A check that finishes well within the debounce window jumps straight
        // to its result line: it is never visible as a spinner.
        let mut state = ProgressState::new(DEBOUNCE);
        let t0 = Instant::now();
        state.register("fast", 4);
        state.start("fast", t0);

        // 30ms in, still inside debounce → nothing visible.
        assert!(state.visible_lines(t0 + Duration::from_millis(30)).is_empty());

        // It finishes clean at 40ms.
        state.finish("fast", 0, Duration::from_millis(40));
        let lines = state.visible_lines(t0 + Duration::from_millis(45));
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].icon, IconKind::Passed);
        assert_eq!(lines[0].text, "fast: 4 files passed [40ms]");
    }

    #[test]
    fn slow_check_promotes_to_spinner_after_debounce() {
        let mut state = ProgressState::new(DEBOUNCE);
        let t0 = Instant::now();
        state.register("slow", 10);
        state.start("slow", t0);

        // Before the debounce: hidden.
        assert!(state.visible_lines(t0 + Duration::from_millis(50)).is_empty());

        // After the debounce: spinner with elapsed + total file count.
        let lines = state.visible_lines(t0 + Duration::from_millis(1000));
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].icon, IconKind::Spinner);
        assert_eq!(lines[0].text, "slow: checking 10 files [1s]");
    }

    #[test]
    fn in_progress_counter_uses_processed_when_known() {
        let mut state = ProgressState::new(DEBOUNCE);
        let t0 = Instant::now();
        state.register("counting", 8);
        state.start("counting", t0);
        state.record_progress("counting", 3);

        let lines = state.visible_lines(t0 + Duration::from_millis(200));
        assert_eq!(lines[0].text, "counting: 3/8 files checked [200ms]");
    }

    #[test]
    fn finished_failing_check_counts_failed_files() {
        let mut state = ProgressState::new(DEBOUNCE);
        state.register("size", 5);
        state.start("size", Instant::now());
        state.finish("size", 2, Duration::from_millis(123));

        let lines = state.visible_lines(Instant::now());
        assert_eq!(lines[0].icon, IconKind::Failed);
        assert_eq!(lines[0].text, "size: 2 files failed [123ms]");
    }

    #[test]
    fn single_file_is_not_pluralized() {
        let mut state = ProgressState::new(DEBOUNCE);
        state.register("one", 1);
        state.start("one", Instant::now());
        state.finish("one", 1, Duration::from_millis(7));
        let lines = state.visible_lines(Instant::now());
        assert_eq!(lines[0].text, "one: 1 file failed [7ms]");
    }

    #[test]
    fn lines_stay_in_registration_order() {
        let mut state = ProgressState::new(DEBOUNCE);
        let t0 = Instant::now();
        for id in ["a", "b", "c"] {
            state.register(id, 2);
            state.start(id, t0);
        }
        // Finish out of order; visible order must follow registration order.
        state.finish("c", 0, Duration::from_millis(1));
        state.finish("a", 0, Duration::from_millis(1));
        state.finish("b", 0, Duration::from_millis(1));
        let now = t0 + Duration::from_millis(5);
        let lines = state.visible_lines(now);
        let ids: Vec<&str> = text_of(&lines).iter().map(|t| t.split(':').next().unwrap()).collect();
        assert_eq!(ids, vec!["a", "b", "c"]);
    }

    #[test]
    fn all_done_reflects_terminal_state() {
        let mut state = ProgressState::new(DEBOUNCE);
        state.register("x", 1);
        state.start("x", Instant::now());
        assert!(!state.all_done());
        state.finish("x", 0, Duration::from_millis(1));
        assert!(state.all_done());
    }

    #[test]
    fn log_lines_drain_once() {
        let mut state = ProgressState::new(DEBOUNCE);
        state.push_log_lines(["line one".to_owned(), "line two".to_owned()]);
        assert_eq!(state.drain_log(), vec!["line one", "line two"]);
        assert!(state.drain_log().is_empty(), "log must be empty after draining");
    }
}
