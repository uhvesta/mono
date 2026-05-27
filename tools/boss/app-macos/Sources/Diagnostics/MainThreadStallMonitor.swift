import AppKit
import Foundation
import os

/// Lightweight main-thread hang watchdog.
///
/// Two timers cooperate:
///   * a **heartbeat** on the main queue stamps a monotonic timestamp
///     (and a coarse "what's frontmost" tag) every `heartbeatIntervalMs`;
///   * a **watchdog** on a background queue polls every `pollIntervalMs`
///     and, when the last heartbeat is more than `thresholdMs` old,
///     suspends the main thread, grabs its backtrace, and records a
///     [[StallRecord]].
///
/// Because detection runs off a *secondary* thread, it fires even while
/// the main thread is fully blocked — which is exactly when we want the
/// backtrace. One record is emitted per stall episode (dedup keyed on
/// the heartbeat sequence number), so a 3-second freeze produces a
/// single entry rather than dozens.
///
/// This is diagnostic instrumentation, not a fix: it tells us whether
/// the main thread is blocked and by what (see the Ghostty pane
/// sluggishness shake), so a regression shows up as a recorded stall
/// instead of folklore. Surfaced in-app via [[UIStallsViewer]].
final class MainThreadStallMonitor: @unchecked Sendable {
    static let shared = MainThreadStallMonitor()

    struct Config: Sendable {
        var heartbeatIntervalMs: Double = 100
        var thresholdMs: Double = 250
        var pollIntervalMs: Double = 50
        var maxFrames: Int = 64
    }

    let log: StallLog
    private let config: Config

    /// The late-threshold the watchdog trips on, in ms — exposed for the
    /// in-app [[UIStallsViewer]] copy.
    var thresholdMs: Double { config.thresholdMs }

    private struct State {
        var lastBeatNanos: UInt64 = 0
        var beat: UInt64 = 0
        /// Last beat we already recorded a stall for, so an ongoing
        /// freeze isn't logged on every watchdog tick. `.max` means
        /// "nothing recorded yet".
        var recordedBeat: UInt64 = .max
        var context: String = "—"
    }
    private let state = OSAllocatedUnfairLock(initialState: State())

    // Timer/port fields are touched only from `start()`/`stop()` on the
    // main thread, so plain stored properties are safe under the
    // `@unchecked Sendable` contract.
    private var heartbeatTimer: DispatchSourceTimer?
    private var watchdogTimer: DispatchSourceTimer?
    private var mainThreadPort: thread_t = 0
    private var running = false
    private let watchdogQueue = DispatchQueue(
        label: "boss.diagnostics.stall-watchdog",
        qos: .utility
    )

    init(config: Config = Config(), log: StallLog = .shared) {
        self.config = config
        self.log = log
    }

    /// Begin monitoring. Must be called on the main thread (it captures
    /// the main thread's Mach port via `mach_thread_self()`). Idempotent.
    func start() {
        dispatchPrecondition(condition: .onQueue(.main))
        guard !running else { return }
        running = true

        // mach_thread_self() returns the *calling* thread's control
        // port; calling it here binds the watchdog to the main thread.
        mainThreadPort = mach_thread_self()

        let now = DispatchTime.now().uptimeNanoseconds
        state.withLock {
            $0.lastBeatNanos = now
            $0.beat = 0
            $0.recordedBeat = .max
        }

        let interval = DispatchTimeInterval.milliseconds(Int(config.heartbeatIntervalMs))
        let hb = DispatchSource.makeTimerSource(queue: .main)
        hb.schedule(deadline: .now() + interval, repeating: interval, leeway: .milliseconds(10))
        hb.setEventHandler { [weak self] in self?.beat() }
        hb.resume()
        heartbeatTimer = hb

        let poll = DispatchTimeInterval.milliseconds(Int(config.pollIntervalMs))
        let wd = DispatchSource.makeTimerSource(queue: watchdogQueue)
        wd.schedule(deadline: .now() + poll, repeating: poll, leeway: .milliseconds(10))
        wd.setEventHandler { [weak self] in self?.tick() }
        wd.resume()
        watchdogTimer = wd
    }

    func stop() {
        dispatchPrecondition(condition: .onQueue(.main))
        heartbeatTimer?.cancel()
        watchdogTimer?.cancel()
        heartbeatTimer = nil
        watchdogTimer = nil
        running = false
    }

    /// Heartbeat — runs on the main queue. Stamps the monotonic clock
    /// and snapshots the frontmost context. Both are cheap; this must
    /// stay trivial so the monitor never becomes the stall it measures.
    private func beat() {
        let now = DispatchTime.now().uptimeNanoseconds
        let ctx = MainActor.assumeIsolated { Self.frontmostContext() }
        state.withLock {
            $0.lastBeatNanos = now
            $0.beat &+= 1
            $0.context = ctx
        }
    }

    /// Watchdog — runs off the main thread. Detects an overdue heartbeat
    /// and records one stall per episode.
    private func tick() {
        let now = DispatchTime.now().uptimeNanoseconds
        let snap = state.withLock {
            (last: $0.lastBeatNanos, beat: $0.beat, recorded: $0.recordedBeat, ctx: $0.context)
        }
        guard let durationMs = StallDetector.stallDurationMs(
            lastHeartbeatNanos: snap.last,
            nowNanos: now,
            thresholdMs: config.thresholdMs
        ) else { return }

        // Already recorded this episode? The heartbeat sequence only
        // advances when the main thread runs, so a still-blocked main
        // thread keeps the same `beat` and we suppress duplicates.
        guard snap.beat != snap.recorded else { return }
        state.withLock { $0.recordedBeat = snap.beat }

        let addresses = MainThreadBacktrace.capture(thread: mainThreadPort, maxFrames: config.maxFrames)
        let frames = MainThreadBacktrace.symbolicate(addresses)
        let record = StallRecord(
            tsEpochMs: Int64(Date().timeIntervalSince1970 * 1000),
            durationMs: durationMs,
            heartbeatIntervalMs: config.heartbeatIntervalMs,
            thresholdMs: config.thresholdMs,
            context: snap.ctx,
            backtrace: frames
        )
        log.record(record)
    }

    /// Coarse "what was active" tag. The frontmost window title surfaces
    /// the worker/agent name in the Boss UI, which is enough to correlate
    /// a stall with the pane the user was interacting with. Must run on
    /// the main actor.
    @MainActor
    static func frontmostContext() -> String {
        let title = NSApp.keyWindow?.title
            ?? NSApp.mainWindow?.title
            ?? NSApp.windows.first(where: { $0.isVisible })?.title
        guard let title, !title.isEmpty else { return "no-frontmost-window" }
        return title
    }
}
