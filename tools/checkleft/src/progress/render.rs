//! Terminal rendering for the progress UI.
//!
//! The layout is two regions sharing stdout:
//!
//! ```text
//! [log area]      findings stream in here as checks complete (scrolls up)
//! [status area]   one pinned line per check, redrawn in place each frame
//! ```
//!
//! Rendering is hand-rolled on top of [`console::Term`] rather than a
//! higher-level progress-bar crate: the status block is redrawn from scratch
//! every frame by clearing exactly the lines drawn last time, appending any new
//! log lines above, and reprinting the status block below. Each status line is
//! truncated to the current terminal width so it occupies exactly one physical
//! row, which keeps the "clear N lines" accounting exact across terminal
//! resizes.
//!
//! [`Renderer`] is a trait so [`draw_frame`] — the per-tick unit of work — can
//! be exercised against an in-memory [`RecordingRenderer`] without a terminal.

use std::io;
use std::time::Instant;

use console::Term;

use super::state::{IconKind, ProgressState, StatusLine};

/// Braille spinner frames, advanced one per render tick.
const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Abstraction over the terminal so [`draw_frame`] is testable without a tty.
pub trait Renderer: Send {
    /// Append `new_log_lines` above the pinned status block, then redraw the
    /// status block from `status_lines`. `tick` advances the spinner animation.
    fn render(&mut self, new_log_lines: &[String], status_lines: &[StatusLine], tick: usize) -> io::Result<()>;

    /// Final flush; leaves the last status block in place on screen.
    fn finalize(&mut self) -> io::Result<()>;
}

/// Drain pending log lines and the current status snapshot from `state` and hand
/// them to `renderer`. This is the whole per-tick step; the live render loop
/// calls it on a timer and tests call it directly.
pub fn draw_frame(renderer: &mut dyn Renderer, state: &mut ProgressState, now: Instant, tick: usize) -> io::Result<()> {
    let log_lines = state.drain_log();
    let status_lines = state.visible_lines(now);
    renderer.render(&log_lines, &status_lines, tick)
}

/// Map a semantic icon (+ animation tick) to a colored glyph. The progress UI
/// only ever runs when color is enabled, so the ANSI codes are unconditional.
fn icon_glyph(icon: IconKind, tick: usize) -> String {
    match icon {
        IconKind::Spinner => {
            let frame = SPINNER_FRAMES[tick % SPINNER_FRAMES.len()];
            format!("\u{1b}[36m{frame}\u{1b}[0m") // cyan
        }
        IconKind::Passed => "\u{1b}[32m✔\u{1b}[0m".to_owned(), // green
        IconKind::Failed => "\u{1b}[31m✖\u{1b}[0m".to_owned(), // red
        IconKind::Skipped => "\u{1b}[2m○\u{1b}[0m".to_owned(), // dim
    }
}

/// Real-terminal renderer backed by [`console::Term`].
pub struct TermRenderer {
    term: Term,
    /// Number of physical rows the status block occupied last frame, so the next
    /// frame can clear exactly those rows before redrawing.
    prev_status_rows: usize,
}

impl TermRenderer {
    pub fn stdout() -> Self {
        Self {
            term: Term::stdout(),
            prev_status_rows: 0,
        }
    }

    fn width(&self) -> usize {
        // `size()` returns (rows, cols); fall back to a sane default off-tty.
        let cols = self.term.size().1 as usize;
        cols.max(1)
    }
}

impl Renderer for TermRenderer {
    fn render(&mut self, new_log_lines: &[String], status_lines: &[StatusLine], tick: usize) -> io::Result<()> {
        // 1. Erase the previous status block so new log lines land above a fresh
        //    redraw rather than scrolling stale status text into history.
        if self.prev_status_rows > 0 {
            self.term.clear_last_lines(self.prev_status_rows)?;
        }

        // 2. Emit new log lines. These scroll up and are permanent, so wrapping
        //    is harmless — no row accounting needed.
        for line in new_log_lines {
            self.term.write_line(line)?;
        }

        // 3. Redraw the status block, truncating each line to one physical row so
        //    `prev_status_rows` stays exact even across a resize.
        let width = self.width();
        for line in status_lines {
            let glyph = icon_glyph(line.icon, tick);
            // Truncation is measured against the visible text (without the
            // glyph's ANSI), then the glyph is prepended; the glyph itself is a
            // single display column.
            let body = console::truncate_str(&line.text, width.saturating_sub(2), "…");
            self.term.write_line(&format!("{glyph} {body}"))?;
        }
        self.prev_status_rows = status_lines.len();
        self.term.flush()
    }

    fn finalize(&mut self) -> io::Result<()> {
        // Leave the final status block in place; just flush.
        self.term.flush()
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;

    /// A captured frame: the log lines appended and the status snapshot drawn.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Frame {
        pub log: Vec<String>,
        pub status: Vec<StatusLine>,
    }

    /// In-memory [`Renderer`] that records every frame, for tests.
    #[derive(Default)]
    pub struct RecordingRenderer {
        pub frames: Vec<Frame>,
        pub finalized: bool,
    }

    impl Renderer for RecordingRenderer {
        fn render(&mut self, new_log_lines: &[String], status_lines: &[StatusLine], _tick: usize) -> io::Result<()> {
            self.frames.push(Frame {
                log: new_log_lines.to_vec(),
                status: status_lines.to_vec(),
            });
            Ok(())
        }

        fn finalize(&mut self) -> io::Result<()> {
            self.finalized = true;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::testing::RecordingRenderer;
    use super::*;

    #[test]
    fn draw_frame_records_log_and_status() {
        let mut state = ProgressState::new(Duration::from_millis(120));
        let t0 = Instant::now();
        state.register("typo", 2);
        state.start("typo", t0);
        state.finish("typo", 0, Duration::from_millis(5));
        state.push_log_lines(["error[typo]: bad".to_owned()]);

        let mut renderer = RecordingRenderer::default();
        draw_frame(&mut renderer, &mut state, t0 + Duration::from_millis(10), 0).unwrap();

        assert_eq!(renderer.frames.len(), 1);
        let frame = &renderer.frames[0];
        assert_eq!(frame.log, vec!["error[typo]: bad".to_owned()]);
        assert_eq!(frame.status.len(), 1);
        assert_eq!(frame.status[0].icon, IconKind::Passed);
        assert_eq!(frame.status[0].text, "typo: 2 files passed [5ms]");
    }

    #[test]
    fn draw_frame_drains_log_so_next_frame_is_clean() {
        let mut state = ProgressState::new(Duration::from_millis(120));
        state.push_log_lines(["one".to_owned()]);
        let mut renderer = RecordingRenderer::default();
        let now = Instant::now();
        draw_frame(&mut renderer, &mut state, now, 0).unwrap();
        draw_frame(&mut renderer, &mut state, now, 1).unwrap();
        assert_eq!(renderer.frames[0].log, vec!["one".to_owned()]);
        assert!(
            renderer.frames[1].log.is_empty(),
            "log must not repeat on the next frame"
        );
    }

    #[test]
    fn spinner_frame_advances_with_tick() {
        // Same icon, different tick → different glyph (animation).
        let g0 = icon_glyph(IconKind::Spinner, 0);
        let g1 = icon_glyph(IconKind::Spinner, 1);
        assert_ne!(g0, g1);
        // Passed/Failed are static regardless of tick.
        assert_eq!(icon_glyph(IconKind::Passed, 0), icon_glyph(IconKind::Passed, 9));
    }
}
