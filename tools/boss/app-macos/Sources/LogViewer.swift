import AppKit
import Foundation
import OSLog
import SwiftUI

// MARK: - Unified log entry model

enum LogSource: String, CaseIterable, Sendable {
    case engine = "engine"
    case app = "app"
    case dispatch = "dispatch"

    var displayName: String {
        switch self {
        case .engine: return "Engine"
        case .app: return "App"
        case .dispatch: return "Dispatch"
        }
    }

    var color: Color {
        switch self {
        case .engine: return .blue
        case .app: return .purple
        case .dispatch: return .orange
        }
    }

    var shortName: String {
        switch self {
        case .engine: return "ENG"
        case .app: return "APP"
        case .dispatch: return "DSP"
        }
    }
}

enum LogLevel: String, CaseIterable, Comparable, Sendable {
    case trace = "TRACE"
    case debug = "DEBUG"
    case info = "INFO"
    case warn = "WARN"
    case error = "ERROR"

    private static let order: [LogLevel] = [.trace, .debug, .info, .warn, .error]

    static func < (lhs: LogLevel, rhs: LogLevel) -> Bool {
        let li = order.firstIndex(of: lhs) ?? 0
        let ri = order.firstIndex(of: rhs) ?? 0
        return li < ri
    }

    var shortName: String {
        switch self {
        case .trace: return "TRC"
        case .debug: return "DBG"
        case .info: return "INF"
        case .warn: return "WRN"
        case .error: return "ERR"
        }
    }

    var color: Color {
        switch self {
        case .trace, .debug: return .secondary
        case .info: return .primary
        case .warn: return .orange
        case .error: return .red
        }
    }

    var displayName: String { rawValue.capitalized }
}

struct LogEntry: Identifiable, Sendable {
    let id = UUID()
    let timestampMs: Int64
    let source: LogSource
    let level: LogLevel
    let message: String
    let target: String
    let fields: [String: String]

    var timestamp: Date {
        Date(timeIntervalSince1970: TimeInterval(timestampMs) / 1000.0)
    }

    var formattedTime: String {
        logTimestampFormatter.string(from: timestamp)
    }
}

// MARK: - Paths

enum LogViewerPaths {
    static func stateRoot() -> URL {
        let fm = FileManager.default
        let appSupport = fm
            .urls(for: .applicationSupportDirectory, in: .userDomainMask)
            .first ?? URL(fileURLWithPath: NSHomeDirectory())
                .appendingPathComponent("Library/Application Support")
        return appSupport.appendingPathComponent("Boss", isDirectory: true)
    }

    /// Structured-JSON tracing output from the engine, consumed by the log viewer.
    static func engineTraceJsonl() -> URL {
        stateRoot().appendingPathComponent("engine-trace.jsonl")
    }
}

// MARK: - Engine trace decoder

enum EngineTraceDecoder {
    /// Parse one JSON line from the engine's tracing JSONL file.
    /// Returns nil for blank or unparseable lines (best-effort, matches writer contract).
    static func decode(line: String) -> LogEntry? {
        let trimmed = line.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty, let data = trimmed.data(using: .utf8) else { return nil }
        guard let any = try? JSONSerialization.jsonObject(with: data),
              let dict = any as? [String: Any] else { return nil }

        guard let tsStr = dict["timestamp"] as? String,
              let levelStr = dict["level"] as? String,
              let fieldsDict = dict["fields"] as? [String: Any],
              let message = fieldsDict["message"] as? String else { return nil }

        guard let tsMs = parseEngineTimestampMs(tsStr) else { return nil }

        let level = LogLevel(rawValue: levelStr) ?? .info
        let target = dict["target"] as? String ?? ""

        var fields: [String: String] = [:]
        for (key, value) in fieldsDict where key != "message" {
            fields[key] = "\(value)"
        }

        return LogEntry(
            timestampMs: tsMs,
            source: .engine,
            level: level,
            message: message,
            target: target,
            fields: fields
        )
    }

    /// Parse RFC 3339 timestamp, tolerating sub-millisecond precision that
    /// tracing-subscriber may emit (e.g. 9-digit nanoseconds). Truncates
    /// fractional seconds to exactly 3 digits before handing to the formatter.
    private static func parseEngineTimestampMs(_ ts: String) -> Int64? {
        let s = normalizeRfc3339FractionalSeconds(ts)
        guard let date = engineTimestampFormatter.date(from: s) else { return nil }
        return Int64(date.timeIntervalSince1970 * 1000)
    }

    private static func normalizeRfc3339FractionalSeconds(_ s: String) -> String {
        guard let dotIdx = s.lastIndex(of: ".") else { return s }
        let afterDot = s.index(after: dotIdx)
        var endFrac = afterDot
        while endFrac < s.endIndex && s[endFrac].isNumber {
            endFrac = s.index(after: endFrac)
        }
        let fracCount = s.distance(from: afterDot, to: endFrac)
        guard fracCount > 3 else { return s }
        let keepEnd = s.index(afterDot, offsetBy: 3)
        return String(s[..<keepEnd]) + String(s[endFrac...])
    }
}

// MARK: - Dispatch event → LogEntry adapter

extension DispatchEvent {
    func toLogEntry() -> LogEntry {
        var fields: [String: String] = [
            "stage": stage,
            "outcome": outcome,
            "execution_id": executionId,
        ]
        if let w = workerId { fields["worker_id"] = w }
        if let e = errorMessage { fields["error"] = e }
        if let wi = workItemId { fields["work_item_id"] = wi }

        let level: LogLevel = outcome == "error" ? .error : (outcome == "skipped" ? .warn : .info)
        let message = "\(stage) → \(outcome)"

        return LogEntry(
            timestampMs: Int64(tsEpochMs),
            source: .dispatch,
            level: level,
            message: message,
            target: "dispatch/\(stage)",
            fields: fields
        )
    }
}

// MARK: - Generic JSONL line tailer

/// Tails an append-only file and delivers raw lines to a callback.
/// Mirrors `DispatchEventsTailer` but is source-agnostic — callers
/// decode lines themselves. I/O runs on a background utility queue.
final class JsonlLineTailer: @unchecked Sendable {
    private let url: URL
    private let onLines: ([String]) -> Void
    private let onEmpty: () -> Void

    private let ioQueue: DispatchQueue
    private var fileHandle: FileHandle?
    private var source: DispatchSourceFileSystemObject?
    private var pollTimer: DispatchSourceTimer?
    private var readOffset: UInt64 = 0
    private var monitoredInode: UInt64?
    private var running = false

    init(url: URL, label: String, onLines: @escaping ([String]) -> Void, onEmpty: @escaping () -> Void) {
        self.url = url
        self.onLines = onLines
        self.onEmpty = onEmpty
        self.ioQueue = DispatchQueue(label: label, qos: .utility)
    }

    func start() {
        ioQueue.async { [weak self] in
            guard let self, !self.running else { return }
            self.running = true
            self.openAndDrain(initial: true)
            self.startPollTimer()
        }
    }

    func stop() {
        ioQueue.async { [weak self] in
            guard let self else { return }
            self.running = false
            self.pollTimer?.cancel()
            self.pollTimer = nil
            self.source?.cancel()
            self.source = nil
            try? self.fileHandle?.close()
            self.fileHandle = nil
            self.readOffset = 0
            self.monitoredInode = nil
        }
    }

    private func startPollTimer() {
        let t = DispatchSource.makeTimerSource(queue: ioQueue)
        t.schedule(deadline: .now() + 2, repeating: 2)
        t.setEventHandler { [weak self] in self?.reconcile() }
        t.resume()
        pollTimer = t
    }

    private func openAndDrain(initial: Bool) {
        let path = url.path
        var statBuf = stat()
        if stat(path, &statBuf) != 0 {
            if initial { let cb = onEmpty; DispatchQueue.main.async { cb() } }
            return
        }

        let handle: FileHandle
        do { handle = try FileHandle(forReadingFrom: url) } catch { return }

        try? fileHandle?.close()
        source?.cancel()

        fileHandle = handle
        readOffset = 0
        monitoredInode = UInt64(statBuf.st_ino)

        let src = DispatchSource.makeFileSystemObjectSource(
            fileDescriptor: handle.fileDescriptor,
            eventMask: [.extend, .write, .delete, .rename],
            queue: ioQueue
        )
        src.setEventHandler { [weak self] in
            guard let self else { return }
            let data = src.data
            if data.contains(.delete) || data.contains(.rename) {
                self.openAndDrain(initial: false)
            } else {
                self.drain()
            }
        }
        src.resume()
        source = src
        drain()
    }

    private func drain() {
        guard let handle = fileHandle else { return }
        let currentSize: UInt64
        do { currentSize = try handle.seekToEnd() } catch { return }
        if currentSize < readOffset { readOffset = 0 }
        if currentSize == readOffset { return }
        do { try handle.seek(toOffset: readOffset) } catch { return }
        guard let data = try? handle.readToEnd(), !data.isEmpty else { return }
        readOffset = currentSize

        guard let chunk = String(data: data, encoding: .utf8) else { return }
        let lines = chunk.split(separator: "\n", omittingEmptySubsequences: true).map(String.init)
        guard !lines.isEmpty else { return }
        let cb = onLines
        DispatchQueue.main.async { cb(lines) }
    }

    private func reconcile() {
        var statBuf = stat()
        guard stat(url.path, &statBuf) == 0 else { return }
        let inode = UInt64(statBuf.st_ino)
        if inode != monitoredInode { openAndDrain(initial: false) }
    }
}

// MARK: - App log watcher (OSLogStore)

/// Polls the system unified log store for entries from the current process.
/// Delivers new entries to `onEntries` every ~2 s on the main queue.
final class AppLogWatcher: @unchecked Sendable {
    private let onEntries: ([LogEntry]) -> Void
    private let ioQueue = DispatchQueue(label: "boss.applog.watch", qos: .utility)
    private var timer: DispatchSourceTimer?
    private var nextQueryDate: Date = Date().addingTimeInterval(-3600)
    private var store: OSLogStore?

    init(onEntries: @escaping ([LogEntry]) -> Void) {
        self.onEntries = onEntries
        self.store = try? OSLogStore(scope: .currentProcessIdentifier)
    }

    func start() {
        ioQueue.async { [weak self] in
            guard let self else { return }
            self.poll()
            self.scheduleTimer()
        }
    }

    func stop() {
        ioQueue.async { [weak self] in
            self?.timer?.cancel()
            self?.timer = nil
        }
    }

    private func scheduleTimer() {
        let t = DispatchSource.makeTimerSource(queue: ioQueue)
        t.schedule(deadline: .now() + 2, repeating: 2)
        t.setEventHandler { [weak self] in self?.poll() }
        t.resume()
        timer = t
    }

    private func poll() {
        guard let store else { return }
        let from = nextQueryDate
        let position = store.position(date: from)

        var entries: [LogEntry] = []
        var maxMs = Int64(from.timeIntervalSince1970 * 1000)

        do {
            let seq = try store.getEntries(with: [], at: position, matching: nil)
            for raw in seq {
                guard let log = raw as? OSLogEntryLog else { continue }
                let level: LogLevel
                switch log.level {
                case .debug: level = .debug
                case .info, .notice: level = .info
                case .error: level = .warn
                case .fault: level = .error
                default: level = .trace
                }
                let ms = Int64(log.date.timeIntervalSince1970 * 1000)
                let target: String
                if log.subsystem.isEmpty {
                    target = log.category.isEmpty ? "app" : log.category
                } else {
                    target = log.category.isEmpty ? log.subsystem : "\(log.subsystem)/\(log.category)"
                }
                entries.append(LogEntry(
                    timestampMs: ms,
                    source: .app,
                    level: level,
                    message: log.composedMessage,
                    target: target,
                    fields: [:]
                ))
                if ms > maxMs { maxMs = ms }
            }
        } catch {}

        nextQueryDate = Date(timeIntervalSince1970: TimeInterval(maxMs) / 1000.0).addingTimeInterval(0.001)

        guard !entries.isEmpty else { return }
        let cb = onEntries
        DispatchQueue.main.async { cb(entries) }
    }
}

// MARK: - View model

@MainActor
final class LogViewerModel: ObservableObject {
    @Published var entries: [LogEntry] = []
    @Published var searchText: String = ""
    @Published var minLevel: LogLevel = .info
    @Published var showEngine: Bool = true
    @Published var showApp: Bool = true
    @Published var showDispatch: Bool = true
    @Published var targetFilter: String = ""
    @Published var autoScroll: Bool = true
    @Published var entryCount: Int = 0

    private var engineTailer: JsonlLineTailer?
    private var dispatchTailer: DispatchEventsTailer?
    private var appWatcher: AppLogWatcher?

    private static let maxEntries = 10_000

    func start() {
        guard engineTailer == nil else { return }
        startEngineTrace()
        startDispatch()
        startAppLogs()
    }

    func stop() {
        engineTailer?.stop()
        engineTailer = nil
        dispatchTailer?.stop()
        dispatchTailer = nil
        appWatcher?.stop()
        appWatcher = nil
    }

    private func startEngineTrace() {
        let tailer = JsonlLineTailer(
            url: LogViewerPaths.engineTraceJsonl(),
            label: "boss.engine-trace.tail",
            onLines: { [weak self] lines in
                let entries = lines.compactMap { EngineTraceDecoder.decode(line: $0) }
                self?.addEntries(entries)
            },
            onEmpty: {}
        )
        engineTailer = tailer
        tailer.start()
    }

    private func startDispatch() {
        let tailer = DispatchEventsTailer(
            url: DispatchEventsPaths.currentJsonl(),
            onEvents: { [weak self] events in
                let entries = events.map { $0.toLogEntry() }
                self?.addEntries(entries)
            },
            onEmpty: {}
        )
        dispatchTailer = tailer
        tailer.start()
    }

    private func startAppLogs() {
        let watcher = AppLogWatcher { [weak self] newEntries in
            self?.addEntries(newEntries)
        }
        appWatcher = watcher
        watcher.start()
    }

    private func addEntries(_ new: [LogEntry]) {
        guard !new.isEmpty else { return }
        // Newest first (matches dispatch viewer convention)
        let sorted = new.sorted { $0.timestampMs > $1.timestampMs }
        entries.insert(contentsOf: sorted, at: 0)
        if entries.count > Self.maxEntries {
            entries.removeLast(entries.count - Self.maxEntries)
        }
        entryCount = entries.count
    }

    var filtered: [LogEntry] {
        let query = searchText.lowercased().trimmingCharacters(in: .whitespacesAndNewlines)
        let target = targetFilter.lowercased().trimmingCharacters(in: .whitespacesAndNewlines)

        return entries.filter { entry in
            switch entry.source {
            case .engine where !showEngine: return false
            case .app where !showApp: return false
            case .dispatch where !showDispatch: return false
            default: break
            }
            if entry.level < minLevel { return false }
            if !target.isEmpty, !entry.target.lowercased().contains(target) { return false }
            if !query.isEmpty {
                let haystack = ([entry.message, entry.target] + Array(entry.fields.values))
                    .joined(separator: " ").lowercased()
                if !haystack.contains(query) { return false }
            }
            return true
        }
    }

    var hasActiveFilters: Bool {
        !searchText.isEmpty || !targetFilter.isEmpty || minLevel != .info
            || !showEngine || !showApp || !showDispatch
    }

    func clearFilters() {
        searchText = ""
        targetFilter = ""
        minLevel = .info
        showEngine = true
        showApp = true
        showDispatch = true
    }
}

// MARK: - SwiftUI views

struct LogViewer: View {
    @StateObject private var model = LogViewerModel()
    @State private var selected: LogEntry?
    @AppStorage("boss.logViewer.visible") private var isOpen = false

    var body: some View {
        VStack(spacing: 0) {
            header
            Divider()
            filterBar
            Divider()
            content
        }
        .onAppear { model.start(); isOpen = true }
        .onDisappear { model.stop(); isOpen = false }
        .frame(minWidth: 1000, minHeight: 520)
    }

    private var header: some View {
        HStack(spacing: 12) {
            Text("Activity Log")
                .font(.headline)
            Spacer()
            Text("\(model.entryCount) entries")
                .font(.caption)
                .foregroundStyle(.secondary)
            Toggle(isOn: $model.autoScroll) {
                Label("Live", systemImage: "livephoto")
                    .font(.caption)
            }
            .toggleStyle(.checkbox)
            .help("Auto-scroll to new entries")
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
    }

    private var filterBar: some View {
        HStack(spacing: 8) {
            // Source toggles
            Text("Source:").font(.caption).foregroundStyle(.secondary)
            SourceBadgeToggle(label: "Engine", color: LogSource.engine.color, on: $model.showEngine)
            SourceBadgeToggle(label: "App", color: LogSource.app.color, on: $model.showApp)
            SourceBadgeToggle(label: "Dispatch", color: LogSource.dispatch.color, on: $model.showDispatch)

            Divider().frame(height: 18)

            Text("Level:").font(.caption).foregroundStyle(.secondary)
            Picker("", selection: $model.minLevel) {
                ForEach(LogLevel.allCases, id: \.self) { level in
                    Text(level.displayName).tag(level)
                }
            }
            .pickerStyle(.menu)
            .labelsHidden()
            .frame(width: 86)

            Divider().frame(height: 18)

            TextField("target/module", text: $model.targetFilter)
                .textFieldStyle(.roundedBorder)
                .frame(width: 150)

            TextField("search message + fields", text: $model.searchText)
                .textFieldStyle(.roundedBorder)
                .frame(width: 240)

            Button("Clear") { model.clearFilters() }
                .buttonStyle(.borderless)
                .disabled(!model.hasActiveFilters)
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 6)
    }

    @ViewBuilder
    private var content: some View {
        let rows = model.filtered
        if rows.isEmpty && model.entries.isEmpty {
            VStack(spacing: 10) {
                ProgressView()
                Text("Waiting for log entries…")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        } else if rows.isEmpty {
            Text("No entries match the current filters")
                .foregroundStyle(.secondary)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
        } else {
            HStack(spacing: 0) {
                logTable(rows: rows)
                    .frame(maxWidth: .infinity)
                if let sel = selected {
                    Divider()
                    LogEntryDetailPanel(entry: sel) { selected = nil }
                        .frame(width: 360)
                }
            }
        }
    }

    private func logTable(rows: [LogEntry]) -> some View {
        Table(rows, selection: Binding(
            get: { selected?.id },
            set: { newID in
                selected = newID.flatMap { id in rows.first { $0.id == id } }
            }
        )) {
            TableColumn("Time") { entry in
                Text(entry.formattedTime)
                    .font(.system(.caption, design: .monospaced))
            }
            .width(min: 100, ideal: 115)

            TableColumn("Src") { entry in
                Text(entry.source.shortName)
                    .font(.caption.weight(.semibold))
                    .foregroundStyle(entry.source.color)
            }
            .width(min: 36, ideal: 42)

            TableColumn("Lvl") { entry in
                Text(entry.level.shortName)
                    .font(.caption.weight(.semibold))
                    .foregroundStyle(entry.level.color)
            }
            .width(min: 32, ideal: 38)

            TableColumn("Target") { entry in
                Text(entry.target)
                    .font(.system(.caption, design: .monospaced))
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .truncationMode(.middle)
            }
            .width(min: 120, ideal: 180)

            TableColumn("Message") { entry in
                Text(entry.message)
                    .font(.system(.caption, design: .monospaced))
                    .lineLimit(1)
                    .truncationMode(.tail)
            }
        }
    }
}

// MARK: - Detail panel

private struct LogEntryDetailPanel: View {
    let entry: LogEntry
    let onClose: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack {
                VStack(alignment: .leading, spacing: 2) {
                    HStack(spacing: 6) {
                        Text(entry.source.displayName)
                            .font(.caption.weight(.semibold))
                            .foregroundStyle(entry.source.color)
                        Text(entry.level.displayName)
                            .font(.caption.weight(.semibold))
                            .foregroundStyle(entry.level.color)
                    }
                    Text(entry.formattedTime)
                        .font(.system(.caption, design: .monospaced))
                        .foregroundStyle(.secondary)
                }
                Spacer()
                Button(action: onClose) {
                    Image(systemName: "xmark.circle.fill")
                }
                .buttonStyle(.borderless)
            }

            Divider()

            if !entry.target.isEmpty {
                LabeledDetailRow(label: "target", value: entry.target)
            }

            Text(entry.message)
                .font(.system(.caption, design: .monospaced))
                .textSelection(.enabled)
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(6)
                .background(
                    RoundedRectangle(cornerRadius: 5, style: .continuous)
                        .fill(Color.gray.opacity(0.08))
                )

            if !entry.fields.isEmpty {
                Divider()
                ForEach(entry.fields.sorted(by: { $0.key < $1.key }), id: \.key) { key, value in
                    LabeledDetailRow(label: key, value: value)
                }
            }

            Spacer()

            Button("Copy as JSON") {
                let pb = NSPasteboard.general
                pb.clearContents()
                pb.setString(entryJSON(entry), forType: .string)
            }
            .buttonStyle(.borderless)
            .font(.caption2)
        }
        .padding(10)
    }

    private func entryJSON(_ entry: LogEntry) -> String {
        var dict: [String: Any] = [
            "timestamp_ms": entry.timestampMs,
            "source": entry.source.rawValue,
            "level": entry.level.rawValue,
            "target": entry.target,
            "message": entry.message,
        ]
        if !entry.fields.isEmpty { dict["fields"] = entry.fields }
        guard let data = try? JSONSerialization.data(
            withJSONObject: dict,
            options: [.prettyPrinted, .sortedKeys]
        ), let s = String(data: data, encoding: .utf8) else { return "" }
        return s
    }
}

private struct LabeledDetailRow: View {
    let label: String
    let value: String

    var body: some View {
        HStack(alignment: .top, spacing: 6) {
            Text(label)
                .font(.system(.caption2, design: .monospaced))
                .foregroundStyle(.secondary)
                .frame(width: 90, alignment: .trailing)
            Text(value)
                .font(.system(.caption2, design: .monospaced))
                .textSelection(.enabled)
                .frame(maxWidth: .infinity, alignment: .leading)
                .lineLimit(3)
        }
    }
}

// MARK: - Filter UI helpers

private struct SourceBadgeToggle: View {
    let label: String
    let color: Color
    @Binding var on: Bool

    var body: some View {
        Button(action: { on.toggle() }) {
            Text(label)
                .font(.system(.caption, design: .rounded).weight(.semibold))
                .foregroundStyle(on ? color : .secondary)
                .padding(.horizontal, 8)
                .padding(.vertical, 3)
                .background(
                    RoundedRectangle(cornerRadius: 6, style: .continuous)
                        .fill(on ? color.opacity(0.15) : Color.gray.opacity(0.08))
                )
                .overlay(
                    RoundedRectangle(cornerRadius: 6, style: .continuous)
                        .stroke(on ? color.opacity(0.5) : Color.gray.opacity(0.2), lineWidth: 1)
                )
        }
        .buttonStyle(.plain)
    }
}

// MARK: - Private formatters

private let logTimestampFormatter: DateFormatter = {
    let f = DateFormatter()
    f.locale = Locale(identifier: "en_US_POSIX")
    f.dateFormat = "HH:mm:ss.SSS"
    return f
}()

// `ISO8601DateFormatter.date(from:)` is documented thread-safe in
// Foundation, but the type is not `Sendable` so Swift 6 strict-
// concurrency rejects a plain `private let` global. The only callers
// are read-only parses; `nonisolated(unsafe)` is the idiomatic Swift 6
// escape hatch when the access pattern is safe by design.
private nonisolated(unsafe) let engineTimestampFormatter: ISO8601DateFormatter = {
    let f = ISO8601DateFormatter()
    f.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
    return f
}()
