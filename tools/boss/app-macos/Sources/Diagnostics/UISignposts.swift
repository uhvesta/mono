import os

/// `os_signpost` instrumentation for the hot interactive paths in the
/// embedded Ghostty panes. Signposts cost ~nothing when Instruments
/// isn't recording, but let us pull per-interaction timing into the
/// "Points of Interest" / "os_signpost" instruments without code
/// changes — and correlate a recorded main-thread stall (see
/// [[MainThreadStallMonitor]]) with which work was in flight.
///
/// Categories map to the suspects called out in the pane-sluggishness
/// shake: selection-drag handling, per-keystroke input, geometry
/// reflow (the O(history) `ghostty_surface_set_size` path), and
/// focus/agent switching.
enum UISignpost {
    static let signposter = OSSignposter(
        subsystem: "com.boss.app",
        category: "ui-interaction"
    )

    /// Names kept as constants so the begin/end/event calls can't drift
    /// out of sync across call sites.
    enum Name {
        static let selectionDrag: StaticString = "selection-drag"
        static let geometryReflow: StaticString = "geometry-reflow"
        static let focusSwitch: StaticString = "focus-switch"
        static let mouseMove: StaticString = "mouse-move"
        static let keystroke: StaticString = "keystroke"
        static let frameDrops: StaticString = "frame-drops"
    }
}
