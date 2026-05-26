import SwiftUI

/// Pure helpers for rendering "how long since the worker last did
/// anything" as a short string. Kept free of SwiftUI so the formatting
/// contract can be unit-tested without hosting a view.
enum WorkerStaleness {
    /// Short "9m" / "45s" / "2h 3m" rendering of the gap between `iso`
    /// (an ISO-8601 `last_event_at`) and `now`. Returns `nil` when the
    /// timestamp is missing or unparseable so callers can fall back to
    /// a time-free message rather than printing a bogus duration.
    static func elapsedShort(since iso: String?, now: Date) -> String? {
        guard let iso, let date = parse(iso) else { return nil }
        let seconds = max(0, now.timeIntervalSince(date))
        return format(seconds: Int(seconds))
    }

    /// Compact human duration. Seconds under a minute, whole minutes
    /// under an hour, then `Hh` (with trailing minutes only when
    /// non-zero) so a long-silent worker reads as `2h 5m` rather than
    /// an unwieldy `125m`.
    static func format(seconds: Int) -> String {
        if seconds < 60 { return "\(seconds)s" }
        let minutes = seconds / 60
        if minutes < 60 { return "\(minutes)m" }
        let hours = minutes / 60
        let remainingMinutes = minutes % 60
        return remainingMinutes == 0 ? "\(hours)h" : "\(hours)h \(remainingMinutes)m"
    }

    /// The engine stamps timestamps as plain `YYYY-MM-DDTHH:MM:SSZ`
    /// (see `format_iso8601_utc` in `live_worker_state.rs`), but accept
    /// fractional seconds too in case a different surface ever feeds
    /// this. `ISO8601DateFormatter.date(from:)` is documented
    /// thread-safe, so the shared formatters are reused.
    static func parse(_ string: String) -> Date? {
        let trimmed = string.trimmingCharacters(in: .whitespaces)
        for formatter in isoFormatters {
            if let date = formatter.date(from: trimmed) { return date }
        }
        return nil
    }

    private nonisolated(unsafe) static let isoFormatters: [ISO8601DateFormatter] = {
        let plain = ISO8601DateFormatter()
        plain.formatOptions = [.withInternetDateTime]
        let fractional = ISO8601DateFormatter()
        fractional.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        return [plain, fractional]
    }()
}

/// Explicit "this worker has paused and isn't making progress"
/// affordance for the live-status row.
///
/// Replaces the old accent-colour-only treatment that flagged
/// `WaitingForInput` with a hue alone — ambiguous (the meaning of the
/// colour was left for the reader to guess) and an accessibility
/// problem (meaning carried by hue cannot be perceived by every user).
///
/// Confirmed trigger: the engine sets `WaitingForInput`
/// (`live_worker_state.rs::apply_event`) when a `Notification` hook
/// fires or a `Stop` arrives with a notification pending — i.e. the
/// worker has stopped and is waiting on a human. There is no
/// time-based "stale" signal feeding this; the elapsed time below is
/// derived in the UI from `last_event_at`, which freezes the moment
/// the worker stops emitting hook events.
struct WorkerWaitingIndicator: View {
    let activity: WorkerActivity?
    /// ISO-8601 `last_event_at` from the worker's `LiveWorkerState` —
    /// wall-clock of the most recent hook event. Frozen once the
    /// worker parks, so `now - lastEventAt` is the time since the
    /// worker last did anything.
    let lastEventAt: String?

    var body: some View {
        if activity == .waitingForInput {
            // Re-evaluate on a slow cadence so the elapsed time in the
            // tooltip stays roughly current even while the worker is
            // silent and the engine pushes no fresh state.
            TimelineView(.periodic(from: .now, by: 30)) { context in
                Image(systemName: "clock.badge.exclamationmark")
                    .font(.caption2.weight(.semibold))
                    .foregroundStyle(.orange)
                    .help(tooltip(now: context.date))
                    .accessibilityLabel(tooltip(now: context.date))
            }
        }
    }

    private func tooltip(now: Date) -> String {
        let meaning = "Worker paused — it has stopped and is waiting for input, so it isn't making progress. Open its pane to check on it."
        guard let elapsed = WorkerStaleness.elapsedShort(since: lastEventAt, now: now) else {
            return meaning
        }
        return "No response for \(elapsed). \(meaning)"
    }
}
