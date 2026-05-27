import AppKit
import QuartzCore
import os

/// (Stretch goal from the pane-sluggishness shake.) Counts how many
/// display refreshes actually serviced our `CADisplayLink` callback
/// during an interaction versus how many the display *should* have
/// produced over the same span. A blocked main thread can't service
/// the callback, so a deficit during a selection drag is a quantitative
/// "frames dropped" signal — much better than "it feels slow."
///
/// Results are emitted as an `os_signpost` event (so they line up with
/// the [[UISignpost]] `selection-drag` interval in Instruments) and to
/// the unified log. The counter is only live for the duration of an
/// interaction, so it adds no steady-state cost.
@MainActor
final class InteractionFrameCounter {
    private static let logger = Logger(subsystem: "com.boss.app", category: "ui-interaction")

    private var displayLink: CADisplayLink?
    private var actualFrames = 0
    private var startTimestamp: CFTimeInterval = 0
    private var lastTimestamp: CFTimeInterval = 0
    /// Nominal seconds-per-frame, refreshed from the link (handles
    /// ProMotion / external displays at non-60 Hz).
    private var nominalFrameInterval: CFTimeInterval = 1.0 / 60.0

    /// Result of an interaction: how many frames the display should have
    /// shown, how many our callback actually serviced, and the deficit.
    struct Result: Equatable {
        let expected: Int
        let actual: Int
        let dropped: Int
    }

    /// Pure arithmetic for the expected/dropped counts, factored out so
    /// the (display-link-free) math is unit-testable. `nonisolated` so the
    /// test suite can call it without hopping to the main actor.
    nonisolated static func tally(
        elapsed: CFTimeInterval,
        frameInterval: CFTimeInterval,
        actualFrames: Int
    ) -> Result? {
        guard elapsed > 0, frameInterval > 0 else { return nil }
        // +1: a frame is serviced at both the first and last tick, so an
        // N-interval span spans N+1 frames.
        let expected = Int((elapsed / frameInterval).rounded()) + 1
        return Result(
            expected: expected,
            actual: actualFrames,
            dropped: max(0, expected - actualFrames)
        )
    }

    /// Start counting on `view`'s display link. Idempotent.
    func begin(on view: NSView) {
        guard displayLink == nil else { return }
        actualFrames = 0
        startTimestamp = 0
        lastTimestamp = 0
        let link = view.displayLink(target: self, selector: #selector(tick(_:)))
        link.add(to: .main, forMode: .common)
        displayLink = link
    }

    /// Stop counting; emit a signpost + log line when frames were
    /// dropped. Returns the tally (or nil if too short to be meaningful).
    @discardableResult
    func end(context: String) -> Result? {
        guard let link = displayLink else { return nil }
        link.invalidate()
        displayLink = nil

        guard let result = Self.tally(
            elapsed: lastTimestamp - startTimestamp,
            frameInterval: nominalFrameInterval,
            actualFrames: actualFrames
        ) else { return nil }

        if result.dropped > 0 {
            UISignpost.signposter.emitEvent(
                UISignpost.Name.frameDrops,
                "\(result.dropped) dropped of \(result.expected) (\(context))"
            )
            Self.logger.notice(
                "selection-drag dropped \(result.dropped, privacy: .public) of \(result.expected, privacy: .public) frames [\(context, privacy: .public)]"
            )
        }
        return result
    }

    @objc private func tick(_ link: CADisplayLink) {
        if startTimestamp == 0 { startTimestamp = link.timestamp }
        lastTimestamp = link.timestamp
        if link.duration > 0 { nominalFrameInterval = link.duration }
        actualFrames += 1
    }
}
