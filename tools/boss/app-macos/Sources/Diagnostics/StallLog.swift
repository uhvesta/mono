import Foundation
import os

/// One recorded main-thread stall: the moment the watchdog noticed the
/// main queue had gone unresponsive for longer than the configured
/// threshold, the symbolicated backtrace of the (blocked) main thread
/// captured at that moment, and a coarse tag for what was frontmost.
///
/// `durationMs` is the stall length **measured at detection** — the
/// watchdog records as soon as it crosses the threshold (so a genuine
/// hang that never resolves is still captured, with its backtrace),
/// rather than waiting for the main thread to recover. It is therefore
/// a lower bound on the true stall: "the main thread had already been
/// blocked at least this long when we looked."
struct StallRecord: Codable, Identifiable, Sendable, Equatable {
    let id: UUID
    /// Wall-clock time the stall was detected, ms since the Unix epoch.
    let tsEpochMs: Int64
    /// Stall duration at detection, in milliseconds (a lower bound).
    let durationMs: Double
    /// Heartbeat cadence the monitor was running at (ms).
    let heartbeatIntervalMs: Double
    /// Late-threshold the watchdog tripped on (ms).
    let thresholdMs: Double
    /// Coarse "what was active" tag — typically the frontmost window
    /// title (which surfaces the pane/agent name in the Boss UI).
    let context: String
    /// Symbolicated main-thread frames, innermost first.
    let backtrace: [String]

    enum CodingKeys: String, CodingKey {
        case id
        case tsEpochMs = "ts_epoch_ms"
        case durationMs = "duration_ms"
        case heartbeatIntervalMs = "heartbeat_interval_ms"
        case thresholdMs = "threshold_ms"
        case context
        case backtrace
    }

    init(
        id: UUID = UUID(),
        tsEpochMs: Int64,
        durationMs: Double,
        heartbeatIntervalMs: Double,
        thresholdMs: Double,
        context: String,
        backtrace: [String]
    ) {
        self.id = id
        self.tsEpochMs = tsEpochMs
        self.durationMs = durationMs
        self.heartbeatIntervalMs = heartbeatIntervalMs
        self.thresholdMs = thresholdMs
        self.context = context
        self.backtrace = backtrace
    }
}

/// Decides whether the main thread is currently stalled given the last
/// heartbeat timestamp and "now". Pure so the threshold contract is
/// unit-testable without standing up a run loop. Times are monotonic
/// nanoseconds (`DispatchTime.uptimeNanoseconds`).
enum StallDetector {
    /// Returns the elapsed time in milliseconds if `now` is more than
    /// `thresholdMs` past `lastHeartbeatNanos`; otherwise `nil`.
    static func stallDurationMs(
        lastHeartbeatNanos: UInt64,
        nowNanos: UInt64,
        thresholdMs: Double
    ) -> Double? {
        guard nowNanos > lastHeartbeatNanos else { return nil }
        let elapsedMs = Double(nowNanos - lastHeartbeatNanos) / 1_000_000.0
        return elapsedMs > thresholdMs ? elapsedMs : nil
    }
}

/// Bounded in-memory ring of recent stalls plus an append-only JSONL
/// mirror on disk. Modeled on [[IpcLog]]: all file I/O runs on a private
/// serial queue so recording never blocks the watchdog, daily files
/// rotate, and `retainDays` of history is kept.
///
/// Files live at:
///   `~/Library/Application Support/Boss/diagnostics/ui-stalls-YYYY-MM-DD.jsonl`
///
/// The on-disk mirror is what a future `bossctl diagnostics ui-stalls
/// --since 5m` would read; the in-memory ring backs the in-app
/// [[UIStallsViewer]] without a disk round-trip.
final class StallLog: @unchecked Sendable {
    static let shared: StallLog = {
        let appSupport = FileManager.default
            .urls(for: .applicationSupportDirectory, in: .userDomainMask)
            .first!
        let dir = appSupport.appendingPathComponent("Boss/diagnostics", isDirectory: true)
        return StallLog(directory: dir.path)
    }()

    private let ring = OSAllocatedUnfairLock(initialState: [StallRecord]())
    private let capacity: Int
    private let retainDays: Int

    /// `nil` directory means in-memory only (used by tests).
    private let directory: String?
    private let queue = DispatchQueue(label: "Boss.StallLog")
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

    init(directory: String?, capacity: Int = 256, retainDays: Int = 7) {
        self.directory = directory
        self.capacity = max(1, capacity)
        self.retainDays = retainDays
    }

    /// Append a stall to the ring (synchronous, lock-guarded) and queue
    /// the JSONL line for the on-disk mirror (asynchronous).
    func record(_ rec: StallRecord) {
        ring.withLock { buf in
            buf.append(rec)
            if buf.count > capacity {
                buf.removeFirst(buf.count - capacity)
            }
        }

        guard directory != nil,
              let json = try? Self.encoder.encode(rec) else { return }
        let lineData = json + Data([0x0A])
        let when = Date(timeIntervalSince1970: Double(rec.tsEpochMs) / 1000.0)
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

    /// Newest-last snapshot of the ring.
    func snapshot() -> [StallRecord] {
        ring.withLock { $0 }
    }

    /// Stalls detected on or after `since`, newest last.
    func recent(since: Date) -> [StallRecord] {
        let cutoffMs = Int64(since.timeIntervalSince1970 * 1000)
        return ring.withLock { $0.filter { $0.tsEpochMs >= cutoffMs } }
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

        let path = (directory as NSString).appendingPathComponent("ui-stalls-\(dateStr).jsonl")
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
            guard name.hasPrefix("ui-stalls-"), name.hasSuffix(".jsonl") else { continue }
            // "ui-stalls-YYYY-MM-DD.jsonl" → "YYYY-MM-DD"
            let dateStr = String(name.dropFirst("ui-stalls-".count).dropLast(".jsonl".count))
            if dateStr < cutoffStr {
                let fullPath = (directory as NSString).appendingPathComponent(name)
                try? FileManager.default.removeItem(atPath: fullPath)
            }
        }
    }

    // MARK: - Plain-text dump

    /// Render `records` as a human-readable report suitable for pasting
    /// into a bug. Pure so the format is unit-testable. Records are
    /// printed newest-first.
    static func formattedDump(_ records: [StallRecord], generatedAt: Date = Date()) -> String {
        let iso = ISO8601DateFormatter()
        iso.formatOptions = [.withInternetDateTime]
        var out = "Boss UI main-thread stall report\n"
        out += "generated: \(iso.string(from: generatedAt))\n"
        out += "stalls: \(records.count)\n"
        if records.isEmpty {
            out += "\nNo stalls recorded in range. 🎉\n"
            return out
        }
        for rec in records.reversed() {
            let when = iso.string(from: Date(timeIntervalSince1970: Double(rec.tsEpochMs) / 1000.0))
            out += "\n"
            out += String(format: "── %@  ≥%.0f ms  [%@]\n", when, rec.durationMs, rec.context)
            if rec.backtrace.isEmpty {
                out += "   (no backtrace captured)\n"
            } else {
                for frame in rec.backtrace {
                    out += "   \(frame)\n"
                }
            }
        }
        return out
    }
}
