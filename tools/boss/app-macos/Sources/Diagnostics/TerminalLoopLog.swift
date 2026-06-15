import Foundation
import os

/// Per-pane liveness snapshot captured by the [[TerminalLoopMonitor]]
/// sampler. The fields are exactly the Swift-observable signals that
/// discriminate the high-CPU hypotheses for a worker terminal pane:
///
///   * `processExited` / `pidAlive` — is the pty at EOF / the child dead?
///     A pane that is dead but still driving the app loop is the dead-fd
///     busy-spin suspect.
///   * `foregroundPid` / `runId` / `slotId` — pane→slot→worker
///     correlation, so a hot/dead pane ties back to a worker run (and the
///     engine-side reap reconciliation, #1507).
///   * `contentChanged` — only populated in deep mode (an extra viewport
///     read per pane). Distinguishes a genuine output flood (content
///     keeps changing) from a dead-fd spin (content frozen).
///
/// libghostty's kqueue `Loop.tick` runs on its own IO threads inside the
/// prebuilt `GhosttyKit.xcframework`, so a true per-surface tick counter
/// is not reachable from Swift; these proxies are.
struct PaneLoopSample: Codable, Identifiable, Sendable, Equatable {
    /// Stable per-pane id — the `TerminalPaneSession.id` ("boss" or
    /// "run-<runId>").
    let paneId: String
    /// "boss" or "worker".
    let role: String
    /// Worker slot id for worker panes; nil for the Boss pane.
    let slotId: Int?
    /// Worker execution/run id for worker panes; nil for the Boss pane.
    let runId: String?
    /// Human-facing pane title (worker name / task).
    let title: String
    /// `ghostty_surface_foreground_pid`, clamped; 0 when unavailable.
    let foregroundPid: Int32
    /// `kill(pid, 0) == 0` — the foreground process group leader exists.
    /// `false` together with a non-zero pid means the child has gone away.
    let pidAlive: Bool
    /// `ghostty_surface_process_exited` — the pty hit EOF / the child
    /// process exited, even though the surface is still mounted.
    let processExited: Bool
    /// Deep-mode only: did the rendered viewport change since the previous
    /// sample? nil when deep content sampling is off.
    let contentChanged: Bool?

    var id: String { paneId }

    enum CodingKeys: String, CodingKey {
        case paneId = "pane_id"
        case role
        case slotId = "slot_id"
        case runId = "run_id"
        case title
        case foregroundPid = "foreground_pid"
        case pidAlive = "pid_alive"
        case processExited = "process_exited"
        case contentChanged = "content_changed"
    }

    /// True when this pane is a dead-fd-spin suspect: the pty is at EOF,
    /// or it reports a foreground pid that no longer exists.
    var isDeadOrEof: Bool {
        processExited || (foregroundPid != 0 && !pidAlive)
    }

    /// Compact "name[slot N,run abc,pid 123,EOF]" tag for warning lines.
    var describe: String {
        var bits: [String] = []
        if let slotId { bits.append("slot \(slotId)") }
        if let runId { bits.append("run \(runId)") }
        if foregroundPid != 0 { bits.append("pid \(foregroundPid)") }
        if processExited { bits.append("EOF") }
        if foregroundPid != 0 && !pidAlive { bits.append("dead-pid") }
        return "\(title)[\(bits.joined(separator: ","))]"
    }
}

/// One periodic event-loop sample: the app-loop wakeup / tick rates plus
/// a per-pane liveness snapshot, taken once per second by the
/// [[TerminalLoopMonitor]]. A hot `wakeupsPerSec` is the Swift-side
/// signature of a spinning libghostty IO loop; cross-referencing it with
/// the per-pane EOF/pid flags pins which pane (and worker) is responsible.
struct LoopSample: Codable, Identifiable, Sendable, Equatable {
    /// Record discriminator for the shared JSONL mirror.
    let kind: String
    let id: UUID
    /// Wall-clock sample time, ms since the Unix epoch.
    let tsEpochMs: Int64
    /// libghostty wakeup callbacks per second over the sample interval.
    let wakeupsPerSec: Double
    /// `ghostty_app_tick` calls per second over the sample interval.
    let ticksPerSec: Double
    /// Actual elapsed interval the rates were computed over (ms).
    let intervalMs: Double
    /// Per-pane liveness for every registered live pane at sample time.
    let panes: [PaneLoopSample]

    init(
        id: UUID = UUID(),
        tsEpochMs: Int64,
        wakeupsPerSec: Double,
        ticksPerSec: Double,
        intervalMs: Double,
        panes: [PaneLoopSample]
    ) {
        self.kind = "loop"
        self.id = id
        self.tsEpochMs = tsEpochMs
        self.wakeupsPerSec = wakeupsPerSec
        self.ticksPerSec = ticksPerSec
        self.intervalMs = intervalMs
        self.panes = panes
    }

    enum CodingKeys: String, CodingKey {
        case kind
        case id
        case tsEpochMs = "ts_epoch_ms"
        case wakeupsPerSec = "wakeups_per_sec"
        case ticksPerSec = "ticks_per_sec"
        case intervalMs = "interval_ms"
        case panes
    }

    /// Panes that look dead/EOF at sample time — the spin suspects.
    var deadPanes: [PaneLoopSample] { panes.filter(\.isDeadOrEof) }
}

/// One Agents/Work/Designs/Automations tab switch and the pane-grid
/// relayout it provoked. Panes are kept alive across tab switches (the
/// ZStack opacity pattern), so the expectation is `panesTornDown == 0`;
/// this record exists to *confirm or refute* that — and to measure how
/// much relayout the switch actually costs (#4 of the work item).
struct TabSwitchSample: Codable, Identifiable, Sendable, Equatable {
    let kind: String
    let id: UUID
    let tsEpochMs: Int64
    /// Navigation mode switched from / to (raw value, e.g. "Work").
    let from: String
    let to: String
    /// Distinct panes that ran a libghostty reflow within the settle
    /// window after the switch.
    let panesRelaidOut: Int
    /// Panes whose libghostty surface was torn down within the window —
    /// expected to be 0 under the keep-alive design.
    let panesTornDown: Int
    /// Time from the switch to the last observed relayout (ms); 0 when no
    /// relayout fired.
    let settleMs: Double

    init(
        id: UUID = UUID(),
        tsEpochMs: Int64,
        from: String,
        to: String,
        panesRelaidOut: Int,
        panesTornDown: Int,
        settleMs: Double
    ) {
        self.kind = "tab_switch"
        self.id = id
        self.tsEpochMs = tsEpochMs
        self.from = from
        self.to = to
        self.panesRelaidOut = panesRelaidOut
        self.panesTornDown = panesTornDown
        self.settleMs = settleMs
    }

    enum CodingKeys: String, CodingKey {
        case kind
        case id
        case tsEpochMs = "ts_epoch_ms"
        case from
        case to
        case panesRelaidOut = "panes_relaid_out"
        case panesTornDown = "panes_torn_down"
        case settleMs = "settle_ms"
    }
}

/// Pure rate arithmetic, factored out so the counter→rate conversion is
/// unit-testable without standing up timers. Times are monotonic
/// nanoseconds (`DispatchTime.uptimeNanoseconds`); counters are
/// monotonically increasing `UInt64`s.
enum TerminalLoopRate {
    /// Events per second given the counter delta and the elapsed
    /// monotonic interval. Returns 0 for a non-positive interval (clock
    /// went backwards, or no time elapsed) so a divide-by-zero can never
    /// produce a bogus spike.
    static func perSecond(delta: UInt64, elapsedNanos: UInt64) -> Double {
        guard elapsedNanos > 0 else { return 0 }
        return Double(delta) / (Double(elapsedNanos) / 1_000_000_000.0)
    }
}

/// One human-readable warning derived from a [[LoopSample]].
struct LoopWarning: Equatable, Sendable {
    let message: String
}

/// Pure decision logic for "is this sample worth a one-line warning?".
/// Kept separate from the monitor so the verify/refute reasoning is
/// directly unit-testable.
enum TerminalLoopThresholds {
    /// Default app-loop wakeup rate (per second) above which we treat the
    /// loop as hot. A healthy idle/typing terminal sits well below this;
    /// a kqueue spin pushes it into the tens-of-thousands.
    static let defaultWakeupWarnPerSec: Double = 2_000

    /// Build warnings for `sample`. The discrimination the work item asks
    /// for lives here:
    ///   * hot wakeup rate + a dead/EOF pane  → dead-fd spin suspected,
    ///     naming the pane/slot/worker;
    ///   * hot wakeup rate + all panes live   → genuine output flood,
    ///     points away from the dead-fd theory;
    ///   * a dead/EOF pane even below the rate threshold → noted, since a
    ///     dead pane should produce no loop activity at all.
    static func warnings(
        for sample: LoopSample,
        wakeupWarnPerSec: Double = defaultWakeupWarnPerSec
    ) -> [LoopWarning] {
        var out: [LoopWarning] = []
        let dead = sample.deadPanes
        let rate = Int(sample.wakeupsPerSec.rounded())

        if sample.wakeupsPerSec >= wakeupWarnPerSec {
            if dead.isEmpty {
                out.append(LoopWarning(message:
                    "hot app-loop: \(rate) wakeups/s across \(sample.panes.count) live pane(s) — "
                    + "no dead/EOF pane, looks like a genuine output flood (not a dead-fd spin)"))
            } else {
                let names = dead.map(\.describe).joined(separator: ", ")
                out.append(LoopWarning(message:
                    "hot app-loop: \(rate) wakeups/s WITH dead/EOF pane(s) [\(names)] — "
                    + "dead-fd busy-spin SUSPECTED"))
            }
        } else if !dead.isEmpty {
            let names = dead.map(\.describe).joined(separator: ", ")
            out.append(LoopWarning(message:
                "dead/EOF pane(s) present [\(names)] at \(rate) wakeups/s — watch for spin"))
        }
        return out
    }
}

/// Bounded in-memory rings of recent loop + tab-switch samples plus an
/// append-only JSONL mirror on disk. Modeled directly on [[StallLog]]:
/// all file I/O runs on a private serial queue, daily files rotate, and
/// `retainDays` of history is kept.
///
/// Files live at:
///   `~/Library/Application Support/Boss/diagnostics/terminal-loop-YYYY-MM-DD.jsonl`
///
/// Each line carries a `kind` field ("loop" or "tab_switch") so a reader
/// (or a future `bossctl diagnostics terminal-loop --since 5m`) can split
/// the stream. The in-memory rings back the in-app [[TerminalLoopViewer]]
/// without a disk round-trip.
final class TerminalLoopLog: @unchecked Sendable {
    static let shared: TerminalLoopLog = {
        let appSupport = FileManager.default
            .urls(for: .applicationSupportDirectory, in: .userDomainMask)
            .first!
        let dir = appSupport.appendingPathComponent("Boss/diagnostics", isDirectory: true)
        return TerminalLoopLog(directory: dir.path)
    }()

    private let loopRing = OSAllocatedUnfairLock(initialState: [LoopSample]())
    private let tabRing = OSAllocatedUnfairLock(initialState: [TabSwitchSample]())
    private let capacity: Int
    private let retainDays: Int

    /// `nil` directory means in-memory only (used by tests — the work item
    /// forbids touching `~/Library/Application Support/Boss` from tests).
    private let directory: String?
    private let queue = DispatchQueue(label: "Boss.TerminalLoopLog")
    private var currentDate = ""
    private var fileHandle: FileHandle?
    private let dateFormatter: DateFormatter = {
        let f = DateFormatter()
        f.dateFormat = "yyyy-MM-dd"
        f.timeZone = TimeZone(identifier: "UTC")
        return f
    }()

    private static let encoder: JSONEncoder = {
        let e = JSONEncoder()
        e.outputFormatting = [.sortedKeys]
        return e
    }()

    init(directory: String?, capacity: Int = 600, retainDays: Int = 7) {
        self.directory = directory
        self.capacity = max(1, capacity)
        self.retainDays = retainDays
    }

    /// Append a loop sample to the ring (synchronous, lock-guarded) and
    /// queue its JSONL line for the on-disk mirror (asynchronous).
    func record(_ sample: LoopSample) {
        loopRing.withLock { buf in
            buf.append(sample)
            if buf.count > capacity {
                buf.removeFirst(buf.count - capacity)
            }
        }
        writeLine(of: sample, at: sample.tsEpochMs)
    }

    /// Append a tab-switch sample to its ring + on-disk mirror.
    func record(_ sample: TabSwitchSample) {
        tabRing.withLock { buf in
            buf.append(sample)
            if buf.count > capacity {
                buf.removeFirst(buf.count - capacity)
            }
        }
        writeLine(of: sample, at: sample.tsEpochMs)
    }

    private func writeLine(of value: some Encodable, at tsEpochMs: Int64) {
        guard directory != nil,
              let json = try? Self.encoder.encode(value) else { return }
        let lineData = json + Data([0x0A])
        let when = Date(timeIntervalSince1970: Double(tsEpochMs) / 1000.0)
        queue.async { [self] in
            let dateStr = dateFormatter.string(from: when)
            if dateStr != currentDate || fileHandle == nil {
                if dateStr != currentDate {
                    pruneOldFiles()
                }
                openFile(dateStr: dateStr)
            }
            fileHandle?.write(lineData)
        }
    }

    /// Newest-last snapshot of the loop ring.
    func loopSnapshot() -> [LoopSample] {
        loopRing.withLock { $0 }
    }

    /// Newest-last snapshot of the tab-switch ring.
    func tabSnapshot() -> [TabSwitchSample] {
        tabRing.withLock { $0 }
    }

    /// Loop samples on or after `since`, newest last.
    func recentLoops(since: Date) -> [LoopSample] {
        let cutoffMs = Int64(since.timeIntervalSince1970 * 1000)
        return loopRing.withLock { $0.filter { $0.tsEpochMs >= cutoffMs } }
    }

    /// Tab switches on or after `since`, newest last.
    func recentTabSwitches(since: Date) -> [TabSwitchSample] {
        let cutoffMs = Int64(since.timeIntervalSince1970 * 1000)
        return tabRing.withLock { $0.filter { $0.tsEpochMs >= cutoffMs } }
    }

    /// Block until queued file writes have drained. Test-only helper.
    func flushForTesting() {
        queue.sync {}
    }

    private func openFile(dateStr: String) {
        guard let directory else { return }
        fileHandle?.closeFile()
        fileHandle = nil

        do {
            try FileManager.default.createDirectory(
                atPath: directory,
                withIntermediateDirectories: true
            )
        } catch {
            return
        }

        let path = (directory as NSString).appendingPathComponent("terminal-loop-\(dateStr).jsonl")
        if !FileManager.default.fileExists(atPath: path) {
            FileManager.default.createFile(atPath: path, contents: nil)
        }
        guard let handle = FileHandle(forWritingAtPath: path) else { return }
        handle.seekToEndOfFile()
        fileHandle = handle
        currentDate = dateStr
    }

    private func pruneOldFiles() {
        guard let directory else { return }
        let cutoff = Date().addingTimeInterval(-Double(retainDays) * 86_400)
        let cutoffStr = dateFormatter.string(from: cutoff)

        guard let entries = try? FileManager.default.contentsOfDirectory(atPath: directory) else {
            return
        }
        for name in entries {
            guard name.hasPrefix("terminal-loop-"), name.hasSuffix(".jsonl") else { continue }
            // "terminal-loop-YYYY-MM-DD.jsonl" → "YYYY-MM-DD"
            let dateStr = String(name.dropFirst("terminal-loop-".count).dropLast(".jsonl".count))
            if dateStr < cutoffStr {
                let fullPath = (directory as NSString).appendingPathComponent(name)
                try? FileManager.default.removeItem(atPath: fullPath)
            }
        }
    }

    // MARK: - Plain-text dump

    /// Render loop + tab-switch samples as a human-readable report for
    /// pasting into a bug. Pure so the format is unit-testable. Samples
    /// are printed newest-first.
    static func formattedDump(
        loops: [LoopSample],
        tabSwitches: [TabSwitchSample],
        generatedAt: Date = Date()
    ) -> String {
        let iso = ISO8601DateFormatter()
        iso.formatOptions = [.withInternetDateTime]
        var out = "Boss terminal event-loop diagnostics\n"
        out += "generated: \(iso.string(from: generatedAt))\n"
        out += "loop samples: \(loops.count)   tab switches: \(tabSwitches.count)\n"

        out += "\n== Tab switches (newest first) ==\n"
        if tabSwitches.isEmpty {
            out += "  (none recorded)\n"
        } else {
            for t in tabSwitches.reversed() {
                let when = iso.string(from: Date(timeIntervalSince1970: Double(t.tsEpochMs) / 1000.0))
                out += String(
                    format: "  %@  %@ → %@   relaid-out %d, torn-down %d, settle %.0f ms\n",
                    when, t.from, t.to, t.panesRelaidOut, t.panesTornDown, t.settleMs
                )
            }
        }

        out += "\n== Loop samples (newest first) ==\n"
        if loops.isEmpty {
            out += "  (none recorded)\n"
            return out
        }
        for s in loops.reversed() {
            let when = iso.string(from: Date(timeIntervalSince1970: Double(s.tsEpochMs) / 1000.0))
            out += String(
                format: "  %@  wakeups %.0f/s  ticks %.0f/s  panes %d\n",
                when, s.wakeupsPerSec, s.ticksPerSec, s.panes.count
            )
            for p in s.panes {
                let pid = p.foregroundPid == 0 ? "—" : String(p.foregroundPid)
                let live = p.foregroundPid == 0 ? "—" : (p.pidAlive ? "alive" : "DEAD")
                let eof = p.processExited ? "EOF" : "ok"
                var line = String(
                    format: "      %@  pid %@ (%@)  pty %@",
                    p.describe, pid, live, eof
                )
                if let changed = p.contentChanged {
                    line += changed ? "  content+" : "  content="
                }
                out += line + "\n"
            }
        }
        return out
    }
}
