import AppKit
import Foundation
import os

/// A pane that can be sampled for event-loop liveness. Implemented by
/// `GhosttyTerminalHostView`, which owns the libghostty surface and is the
/// only type that can read `ghostty_surface_process_exited` /
/// `ghostty_surface_foreground_pid`. Main-actor because the surface and
/// the backing session are main-actor state.
@MainActor
protocol PaneLoopProbe: AnyObject {
    /// Stable pane id (`TerminalPaneSession.id`).
    var loopPaneId: String { get }
    /// Capture this pane's current liveness, or nil if it has no live
    /// surface. `includeContent` enables the extra viewport read used to
    /// tell a frozen (spinning) pane from one producing fresh output.
    func loopProbe(includeContent: Bool) -> PaneLoopSample?
}

/// Diagnostic monitor for the embedded-terminal event loop and the
/// tab-switch pane-grid relayout — the instrumentation arm of the
/// high-CPU investigation.
///
/// Ghostty ships as a prebuilt `GhosttyKit.xcframework`, so its kqueue
/// `Loop.tick` (the symbol that dominated the CPU sample) runs on
/// libghostty's own IO threads and cannot be counted at the Zig level.
/// This monitor instead counts the Swift-visible *app-loop* activity that
/// a spinning surface loop necessarily drives — every IO-loop wakeup that
/// has work for the app posts to the bounded `App.Message` mailbox and
/// fires libghostty's `wakeup` callback, which we count — and once per
/// second cross-references that rate against each pane's pty/EOF and pid
/// liveness. A hot wakeup rate paired with a dead/EOF pane is the
/// dead-fd-spin signature; a hot rate with all panes live points at a
/// genuine output flood instead.
///
/// Cost discipline (the instrumentation must not distort the profile it
/// measures):
///   * `recordWakeup` / `recordTick` are two unfair-lock increments on
///     paths libghostty already drives — negligible next to the
///     `OperationQueue.main` hop the wakeup path already performs, and on
///     a path the spin cannot bypass.
///   * the sampler runs at 1 Hz and, by default, does only cheap C calls
///     (`process_exited`, `foreground_pid`, `kill(pid,0)`) per pane.
///   * the extra viewport read (deep content mode) and sampling itself
///     are toggleable (`UserDefaults` keys, surfaced in
///     [[TerminalLoopViewer]]).
///
/// Surfaced without a debugger via os.Logger warnings (subsystem
/// `com.boss.app`, category `terminal-loop`), the on-disk JSONL mirror in
/// [[TerminalLoopLog]], and the in-app [[TerminalLoopViewer]].
final class TerminalLoopMonitor: @unchecked Sendable {
    static let shared = TerminalLoopMonitor()

    /// `UserDefaults` key — master switch for the 1 Hz sampler. Default
    /// on so warnings appear without any setup; flip off to fully silence
    /// the instrumentation if you suspect it is perturbing a measurement.
    static let samplingEnabledKey = "boss.terminalLoop.sampling"
    /// `UserDefaults` key — deep content sampling (an extra per-pane
    /// viewport read each second to detect output flow). Default off.
    static let deepContentKey = "boss.terminalLoop.deepContent"

    struct Config: Sendable {
        var sampleIntervalSec: TimeInterval = 1.0
        /// Window after a tab switch in which relayouts/teardowns are
        /// attributed to that switch.
        var tabSwitchSettleSec: TimeInterval = 0.7
        var wakeupWarnPerSec: Double = TerminalLoopThresholds.defaultWakeupWarnPerSec
        /// Minimum spacing between emitted warning lines, so a sustained
        /// spin logs periodically rather than every second.
        var warnThrottleSec: TimeInterval = 5.0
    }

    let log: TerminalLoopLog
    private let config: Config
    private let logger = Logger(subsystem: "com.boss.app", category: "terminal-loop")

    /// The late-threshold the sampler warns on, exposed for the viewer.
    var wakeupWarnPerSec: Double { config.wakeupWarnPerSec }

    // MARK: Hot-path counters (any thread)

    private let wakeups = OSAllocatedUnfairLock(initialState: UInt64(0))
    private let ticks = OSAllocatedUnfairLock(initialState: UInt64(0))

    /// Count one libghostty `wakeup` callback. Called from
    /// `GhosttyRuntime.wakeup` on whatever thread libghostty signals.
    func recordWakeup() { wakeups.withLock { $0 &+= 1 } }

    /// Count one `ghostty_app_tick`. Called from `GhosttyRuntime.tick`.
    func recordTick() { ticks.withLock { $0 &+= 1 } }

    // MARK: Sampler state (main actor)

    private final class WeakPaneBox {
        weak var probe: (any PaneLoopProbe)?
        init(_ probe: any PaneLoopProbe) { self.probe = probe }
    }

    @MainActor private var panes: [WeakPaneBox] = []
    @MainActor private var timer: Timer?
    @MainActor private var lastSampleNanos: UInt64 = 0
    @MainActor private var lastWakeups: UInt64 = 0
    @MainActor private var lastTicks: UInt64 = 0
    @MainActor private var lastWarnNanos: UInt64 = 0
    @MainActor private var activeTabSwitch: TabSwitchCollector?

    init(config: Config = Config(), log: TerminalLoopLog = .shared) {
        self.config = config
        self.log = log
    }

    // MARK: Lifecycle

    /// Start the 1 Hz sampler. Idempotent; call once at launch (next to
    /// `MainThreadStallMonitor.shared.start()`).
    @MainActor
    func start() {
        guard timer == nil else { return }
        lastSampleNanos = DispatchTime.now().uptimeNanoseconds
        lastWakeups = wakeups.withLock { $0 }
        lastTicks = ticks.withLock { $0 }

        let t = Timer.scheduledTimer(
            withTimeInterval: config.sampleIntervalSec,
            repeats: true
        ) { [weak self] _ in
            Task { @MainActor [weak self] in self?.sample() }
        }
        t.tolerance = config.sampleIntervalSec * 0.2
        timer = t
    }

    @MainActor
    func stop() {
        timer?.invalidate()
        timer = nil
    }

    // MARK: Pane registry

    @MainActor
    func register(_ probe: any PaneLoopProbe) {
        // Drop any dead boxes and avoid duplicate registration.
        panes.removeAll { $0.probe == nil || $0.probe === probe }
        panes.append(WeakPaneBox(probe))
    }

    @MainActor
    func unregister(_ probe: any PaneLoopProbe) {
        panes.removeAll { $0.probe == nil || $0.probe === probe }
    }

    // MARK: Sampling

    @MainActor
    private func sample() {
        guard UserDefaults.standard.object(forKey: Self.samplingEnabledKey) as? Bool ?? true else {
            return
        }

        let now = DispatchTime.now().uptimeNanoseconds
        let elapsedNanos = now > lastSampleNanos ? now - lastSampleNanos : 0
        let wakeupsNow = wakeups.withLock { $0 }
        let ticksNow = ticks.withLock { $0 }
        let dWake = wakeupsNow &- lastWakeups
        let dTick = ticksNow &- lastTicks
        lastSampleNanos = now
        lastWakeups = wakeupsNow
        lastTicks = ticksNow

        let deep = UserDefaults.standard.bool(forKey: Self.deepContentKey)
        var paneSamples: [PaneLoopSample] = []
        panes.removeAll { $0.probe == nil }
        for box in panes {
            if let p = box.probe?.loopProbe(includeContent: deep) {
                paneSamples.append(p)
            }
        }

        let sample = LoopSample(
            tsEpochMs: Int64(Date().timeIntervalSince1970 * 1000),
            wakeupsPerSec: TerminalLoopRate.perSecond(delta: dWake, elapsedNanos: elapsedNanos),
            ticksPerSec: TerminalLoopRate.perSecond(delta: dTick, elapsedNanos: elapsedNanos),
            intervalMs: Double(elapsedNanos) / 1_000_000.0,
            panes: paneSamples
        )
        log.record(sample)

        let warnings = TerminalLoopThresholds.warnings(
            for: sample,
            wakeupWarnPerSec: config.wakeupWarnPerSec
        )
        emit(warnings, now: now)
    }

    @MainActor
    private func emit(_ warnings: [LoopWarning], now: UInt64) {
        guard !warnings.isEmpty else { return }
        let throttleNanos = UInt64(config.warnThrottleSec * 1_000_000_000)
        guard lastWarnNanos == 0 || now &- lastWarnNanos >= throttleNanos else { return }
        lastWarnNanos = now
        for w in warnings {
            logger.warning("terminal-loop: \(w.message, privacy: .public)")
        }
    }

    // MARK: Tab-switch / relayout instrumentation

    /// Collects the relayout/teardown burst attributed to a single tab
    /// switch. Main-actor: every mutation happens on the main run loop.
    @MainActor
    private final class TabSwitchCollector {
        let from: String
        let to: String
        let startNanos: UInt64
        let tsEpochMs: Int64
        private var relaidOut: Set<String> = []
        private var tornDown: Set<String> = []
        private var lastRelayoutNanos: UInt64 = 0

        init(from: String, to: String, startNanos: UInt64, tsEpochMs: Int64) {
            self.from = from
            self.to = to
            self.startNanos = startNanos
            self.tsEpochMs = tsEpochMs
        }

        func noteRelayout(_ paneId: String, atNanos: UInt64) {
            relaidOut.insert(paneId)
            lastRelayoutNanos = atNanos
        }

        func noteTeardown(_ paneId: String) {
            tornDown.insert(paneId)
        }

        func finalize() -> TabSwitchSample {
            let settleMs = lastRelayoutNanos > startNanos
                ? Double(lastRelayoutNanos - startNanos) / 1_000_000.0
                : 0
            return TabSwitchSample(
                tsEpochMs: tsEpochMs,
                from: from,
                to: to,
                panesRelaidOut: relaidOut.count,
                panesTornDown: tornDown.count,
                settleMs: settleMs
            )
        }
    }

    /// Mark the start of a navigation-mode switch. Pane reflows and
    /// teardowns within `tabSwitchSettleSec` are attributed to it, then a
    /// [[TabSwitchSample]] is recorded and (if anything rebuilt) logged.
    @MainActor
    func noteTabSwitch(from: String, to: String) {
        let collector = TabSwitchCollector(
            from: from,
            to: to,
            startNanos: DispatchTime.now().uptimeNanoseconds,
            tsEpochMs: Int64(Date().timeIntervalSince1970 * 1000)
        )
        activeTabSwitch = collector
        DispatchQueue.main.asyncAfter(deadline: .now() + config.tabSwitchSettleSec) { [weak self] in
            MainActor.assumeIsolated {
                guard let self else { return }
                let result = collector.finalize()
                self.log.record(result)
                // Only clear the slot if a newer switch hasn't replaced us.
                if self.activeTabSwitch === collector { self.activeTabSwitch = nil }
                if result.panesRelaidOut > 0 || result.panesTornDown > 0 {
                    self.logger.notice("""
                    terminal-loop: tab switch \(result.from, privacy: .public) → \
                    \(result.to, privacy: .public) relaid-out \(result.panesRelaidOut) pane(s), \
                    torn-down \(result.panesTornDown), settle \(Int(result.settleMs)) ms
                    """)
                }
            }
        }
    }

    /// Record that `paneId` ran a libghostty reflow. Cheap no-op unless a
    /// tab switch is currently being measured.
    @MainActor
    func recordRelayout(paneId: String) {
        activeTabSwitch?.noteRelayout(paneId, atNanos: DispatchTime.now().uptimeNanoseconds)
    }

    /// Record that `paneId`'s surface was torn down (expected to be 0
    /// during a tab switch under the keep-alive ZStack design).
    @MainActor
    func recordTeardown(paneId: String) {
        activeTabSwitch?.noteTeardown(paneId)
    }
}

/// Lightweight liveness probe of whether `pid` still exists, via
/// `kill(pid, 0)`. `EPERM` means the process exists but we may not signal
/// it (it counts as alive); `ESRCH` means it is gone. Factored out so the
/// host-view probe stays terse.
func pidIsAlive(_ pid: Int32) -> Bool {
    guard pid > 0 else { return false }
    if kill(pid, 0) == 0 { return true }
    return errno == EPERM
}
