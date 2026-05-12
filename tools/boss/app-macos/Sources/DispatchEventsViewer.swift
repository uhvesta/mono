import AppKit
import Foundation
import SwiftUI

// MARK: - DispatchEvent model

/// Mirror of `DispatchEvent` from
/// `tools/boss/engine/src/dispatch_events.rs`. Decoded with relaxed
/// rules — unknown fields are ignored and missing optionals are
/// tolerated so the viewer keeps working if the engine adds fields.
struct DispatchEvent: Identifiable, Hashable {
    let id: UUID = UUID()
    let tsEpochMs: UInt64
    let stage: String
    let outcome: String
    let executionId: String
    let workItemId: String?
    let workerId: String?
    let cubeRepoId: String?
    let cubeLeaseId: String?
    let cubeWorkspaceId: String?
    let errorMessage: String?
    let detailsJSON: String?

    var timestamp: Date {
        Date(timeIntervalSince1970: TimeInterval(tsEpochMs) / 1000.0)
    }

    var outcomeColor: Color {
        switch outcome {
        case "ok": return .secondary
        case "error": return .red
        case "skipped": return .orange
        default: return .secondary
        }
    }

    /// Short tail of an opaque id like `exec_18ae9d258b5872e8_48` →
    /// the trailing component after the last `_`, suitable for a
    /// compact table column.
    static func shortId(_ id: String) -> String {
        guard let underscore = id.lastIndex(of: "_") else { return id }
        return String(id[id.index(after: underscore)...])
    }
}

enum DispatchEventDecoder {
    /// Parse one JSONL line. Returns `nil` for blank lines and lines
    /// that don't decode (the viewer drops them quietly — best-effort
    /// matches the writer's best-effort emit contract).
    static func decode(line: String) -> DispatchEvent? {
        let trimmed = line.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty, let data = trimmed.data(using: .utf8) else { return nil }
        guard let any = try? JSONSerialization.jsonObject(with: data, options: []),
              let dict = any as? [String: Any] else { return nil }

        let ts: UInt64
        if let n = dict["ts_epoch_ms"] as? NSNumber {
            ts = n.uint64Value
        } else { return nil }
        guard let stage = dict["stage"] as? String,
              let outcome = dict["outcome"] as? String,
              let executionId = dict["execution_id"] as? String else { return nil }

        let detailsJSON: String?
        if let details = dict["details"], !(details is NSNull) {
            if let detailsData = try? JSONSerialization.data(
                withJSONObject: details,
                options: [.prettyPrinted, .sortedKeys]
            ) {
                detailsJSON = String(data: detailsData, encoding: .utf8)
            } else {
                detailsJSON = nil
            }
        } else {
            detailsJSON = nil
        }

        return DispatchEvent(
            tsEpochMs: ts,
            stage: stage,
            outcome: outcome,
            executionId: executionId,
            workItemId: dict["work_item_id"] as? String,
            workerId: dict["worker_id"] as? String,
            cubeRepoId: dict["cube_repo_id"] as? String,
            cubeLeaseId: dict["cube_lease_id"] as? String,
            cubeWorkspaceId: dict["cube_workspace_id"] as? String,
            errorMessage: dict["error_message"] as? String,
            detailsJSON: detailsJSON
        )
    }
}

// MARK: - Filesystem paths

enum DispatchEventsPaths {
    /// Boss state root — `~/Library/Application Support/Boss`. The
    /// engine code in `engine/src/main.rs` resolves the same path.
    static func stateRoot() -> URL {
        let fm = FileManager.default
        let appSupport = fm
            .urls(for: .applicationSupportDirectory, in: .userDomainMask)
            .first ?? URL(fileURLWithPath: NSHomeDirectory())
                .appendingPathComponent("Library/Application Support")
        return appSupport.appendingPathComponent("Boss", isDirectory: true)
    }

    /// Source-of-truth JSONL stream.
    static func currentJsonl() -> URL {
        stateRoot()
            .appendingPathComponent("dispatch-events", isDirectory: true)
            .appendingPathComponent("current.jsonl")
    }

    /// Per-execution mirror.
    static func executionJsonl(executionId: String) -> URL {
        stateRoot()
            .appendingPathComponent("executions", isDirectory: true)
            .appendingPathComponent(executionId, isDirectory: true)
            .appendingPathComponent("dispatch.jsonl")
    }
}

// MARK: - Live tail

/// Tails an append-only JSONL file. Re-opens cleanly when the file
/// is rotated (inode changes) or truncated. Read I/O happens on a
/// background queue; new events are delivered to `onEvents` /
/// `onEmpty` after a `DispatchQueue.main.async` hop.
final class DispatchEventsTailer: @unchecked Sendable {
    private let url: URL
    private let onEvents: ([DispatchEvent]) -> Void
    private let onEmpty: () -> Void

    private let ioQueue = DispatchQueue(label: "boss.dispatch-events.tail", qos: .utility)
    private var fileHandle: FileHandle?
    private var source: DispatchSourceFileSystemObject?
    private var readOffset: UInt64 = 0
    /// Re-check the file path every few seconds even when the
    /// monitored handle reports no changes — covers the case where
    /// the file did not exist at start (no handle to monitor) or
    /// where the file was replaced atomically and our handle now
    /// points at an inode that was unlinked.
    private var pollTimer: DispatchSourceTimer?
    private var monitoredInode: UInt64?
    private var running = false

    init(
        url: URL,
        onEvents: @escaping ([DispatchEvent]) -> Void,
        onEmpty: @escaping () -> Void
    ) {
        self.url = url
        self.onEvents = onEvents
        self.onEmpty = onEmpty
    }

    func start() {
        ioQueue.async { [weak self] in
            guard let self else { return }
            if self.running { return }
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
        let timer = DispatchSource.makeTimerSource(queue: ioQueue)
        timer.schedule(deadline: .now() + 2, repeating: 2)
        timer.setEventHandler { [weak self] in
            self?.reconcile()
        }
        pollTimer = timer
        timer.resume()
    }

    /// Open or re-open the file and read whatever is currently in it.
    /// Called both at start and after a rotation/truncation.
    private func openAndDrain(initial: Bool) {
        let path = url.path
        var statBuf = stat()
        if stat(path, &statBuf) != 0 {
            if initial {
                let cb = onEmpty
                DispatchQueue.main.async { cb() }
            }
            return
        }

        let handle: FileHandle
        do {
            handle = try FileHandle(forReadingFrom: url)
        } catch {
            return
        }

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
            self?.handleSourceEvent(events: src.data)
        }
        src.resume()
        source = src

        drain()
    }

    private func handleSourceEvent(events: DispatchSource.FileSystemEvent) {
        if events.contains(.delete) || events.contains(.rename) {
            // File rotated / removed — re-open from scratch.
            openAndDrain(initial: false)
            return
        }
        drain()
    }

    private func drain() {
        guard let handle = fileHandle else { return }
        let currentSize: UInt64
        do {
            currentSize = try handle.seekToEnd()
        } catch {
            return
        }
        if currentSize < readOffset {
            // Truncation. Treat as fresh file.
            readOffset = 0
        }
        if currentSize == readOffset { return }

        do {
            try handle.seek(toOffset: readOffset)
        } catch {
            return
        }

        guard let data = try? handle.readToEnd(), !data.isEmpty else { return }
        readOffset = currentSize

        guard let chunk = String(data: data, encoding: .utf8) else { return }
        var parsed: [DispatchEvent] = []
        for line in chunk.split(separator: "\n", omittingEmptySubsequences: true) {
            if let event = DispatchEventDecoder.decode(line: String(line)) {
                parsed.append(event)
            }
        }
        if parsed.isEmpty { return }
        let cb = onEvents
        DispatchQueue.main.async { cb(parsed) }
    }

    /// Catch the rotate/replace case where our monitored fd points
    /// at an inode that's been unlinked: the dispatch source on a
    /// deleted file may not fire if the new file was created via
    /// `open(O_CREAT)` rather than rename.
    private func reconcile() {
        let path = url.path
        var statBuf = stat()
        if stat(path, &statBuf) != 0 {
            return
        }
        let currentInode = UInt64(statBuf.st_ino)
        if currentInode != monitoredInode {
            openAndDrain(initial: false)
        }
    }
}

// MARK: - View model

@MainActor
final class DispatchEventsViewerModel: ObservableObject {
    @Published var events: [DispatchEvent] = []
    @Published var stageFilter: Set<String> = []
    @Published var outcomeFilter: Set<String> = []
    @Published var searchText: String = ""
    /// Source path is part of the empty state shown when there's no
    /// data; surfacing it makes it obvious which file the viewer is
    /// reading.
    @Published var sourcePath: String
    @Published var sourceMissing: Bool = false
    @Published var lastEventAt: Date?
    @Published var autoScroll: Bool = true

    private var tailer: DispatchEventsTailer?

    static let knownStages: [String] = [
        "request_recorded",
        "worker_claimed",
        "cube_repo_ensured",
        "cube_workspace_leased",
        "cube_change_created",
        "run_started",
        "pane_spawned",
    ]

    static let knownOutcomes: [String] = ["ok", "error", "skipped"]

    init() {
        self.sourcePath = DispatchEventsPaths.currentJsonl().path
    }

    func start() {
        guard tailer == nil else { return }
        let tailer = DispatchEventsTailer(
            url: DispatchEventsPaths.currentJsonl(),
            onEvents: { [weak self] new in
                self?.append(new)
            },
            onEmpty: { [weak self] in
                self?.sourceMissing = true
            }
        )
        self.tailer = tailer
        tailer.start()
    }

    func stop() {
        tailer?.stop()
        tailer = nil
    }

    private func append(_ new: [DispatchEvent]) {
        sourceMissing = false
        // Engine writes oldest-first; keep newest-first in memory so
        // the table can stream the prefix.
        events.insert(contentsOf: new.reversed(), at: 0)
        // Cap memory: the file may grow without bound. 5 000 in-view
        // events is plenty for diagnosis and keeps the UI snappy;
        // the on-disk file remains the authoritative archive.
        if events.count > 5_000 {
            events.removeLast(events.count - 5_000)
        }
        lastEventAt = new.last.map { $0.timestamp } ?? lastEventAt
    }

    var filtered: [DispatchEvent] {
        let stages = stageFilter
        let outcomes = outcomeFilter
        let query = searchText.lowercased().trimmingCharacters(in: .whitespacesAndNewlines)
        return events.filter { event in
            if !stages.isEmpty, !stages.contains(event.stage) { return false }
            if !outcomes.isEmpty, !outcomes.contains(event.outcome) { return false }
            if !query.isEmpty {
                let haystack = [
                    event.executionId,
                    event.workItemId ?? "",
                    event.workerId ?? "",
                    event.errorMessage ?? "",
                    event.stage,
                ].joined(separator: " ").lowercased()
                if !haystack.contains(query) { return false }
            }
            return true
        }
    }

    func toggleStage(_ stage: String) {
        if stageFilter.contains(stage) {
            stageFilter.remove(stage)
        } else {
            stageFilter.insert(stage)
        }
    }

    func toggleOutcome(_ outcome: String) {
        if outcomeFilter.contains(outcome) {
            outcomeFilter.remove(outcome)
        } else {
            outcomeFilter.insert(outcome)
        }
    }

    func clearFilters() {
        stageFilter.removeAll()
        outcomeFilter.removeAll()
        searchText = ""
    }
}

// MARK: - Per-execution timeline

struct DispatchExecutionTimeline {
    let executionId: String
    let events: [DispatchEvent]
    let missing: Bool
}

enum DispatchExecutionLoader {
    /// Load and decode the per-execution mirror. Returns
    /// `missing: true` when the file does not exist.
    static func load(executionId: String) -> DispatchExecutionTimeline {
        let url = DispatchEventsPaths.executionJsonl(executionId: executionId)
        guard let data = try? Data(contentsOf: url),
              let text = String(data: data, encoding: .utf8) else {
            return DispatchExecutionTimeline(
                executionId: executionId,
                events: [],
                missing: true
            )
        }
        var parsed: [DispatchEvent] = []
        for line in text.split(separator: "\n", omittingEmptySubsequences: true) {
            if let event = DispatchEventDecoder.decode(line: String(line)) {
                parsed.append(event)
            }
        }
        // Chronological — earliest first matches the dispatch order.
        parsed.sort { $0.tsEpochMs < $1.tsEpochMs }
        return DispatchExecutionTimeline(
            executionId: executionId,
            events: parsed,
            missing: false
        )
    }
}

// MARK: - SwiftUI views

struct DispatchEventsViewer: View {
    @StateObject private var model = DispatchEventsViewerModel()
    @State private var selected: DispatchEvent?

    var body: some View {
        VStack(spacing: 0) {
            header
            Divider()
            filters
            Divider()
            content
        }
        .onAppear { model.start() }
        .onDisappear { model.stop() }
        .frame(minWidth: 900, minHeight: 520)
    }

    private var header: some View {
        HStack(spacing: 12) {
            Text("Dispatch Events")
                .font(.headline)
            Spacer()
            Text("\(model.events.count) events")
                .font(.caption)
                .foregroundStyle(.secondary)
            if let last = model.lastEventAt {
                Text("last \(timestampFormatter.string(from: last))")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
    }

    private var filters: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 6) {
                Text("Stage").font(.caption).foregroundStyle(.secondary)
                ForEach(DispatchEventsViewerModel.knownStages, id: \.self) { stage in
                    FilterChip(
                        label: stage,
                        active: model.stageFilter.contains(stage),
                        action: { model.toggleStage(stage) }
                    )
                }
            }
            HStack(spacing: 6) {
                Text("Outcome").font(.caption).foregroundStyle(.secondary)
                ForEach(DispatchEventsViewerModel.knownOutcomes, id: \.self) { outcome in
                    FilterChip(
                        label: outcome,
                        active: model.outcomeFilter.contains(outcome),
                        action: { model.toggleOutcome(outcome) }
                    )
                }
                Spacer()
                TextField("search execution / work item / error", text: $model.searchText)
                    .textFieldStyle(.roundedBorder)
                    .frame(width: 320)
                Button("Clear") {
                    model.clearFilters()
                }
                .buttonStyle(.borderless)
                .disabled(model.stageFilter.isEmpty && model.outcomeFilter.isEmpty && model.searchText.isEmpty)
            }
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
    }

    @ViewBuilder
    private var content: some View {
        if model.sourceMissing && model.events.isEmpty {
            emptyState
        } else if model.filtered.isEmpty && model.events.isEmpty {
            ProgressView("Loading dispatch events…")
                .frame(maxWidth: .infinity, maxHeight: .infinity)
        } else {
            HStack(spacing: 0) {
                eventTable
                    .frame(maxWidth: .infinity)
                if let selected {
                    Divider()
                    DispatchExecutionDrawer(event: selected) {
                        self.selected = nil
                    }
                    .frame(width: 380)
                }
            }
        }
    }

    private var emptyState: some View {
        VStack(spacing: 10) {
            Text("No dispatch events yet")
                .font(.headline)
            Text("Engine has not dispatched any work since the events file was created. Drag a chore into Doing to generate events.")
                .multilineTextAlignment(.center)
                .foregroundStyle(.secondary)
                .frame(maxWidth: 480)
            Text(model.sourcePath)
                .font(.system(.caption, design: .monospaced))
                .foregroundStyle(.secondary)
                .textSelection(.enabled)
        }
        .padding()
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    private var eventTable: some View {
        let rows = model.filtered
        return Table(rows, selection: Binding(
            get: { selected?.id },
            set: { newID in
                if let newID, let match = rows.first(where: { $0.id == newID }) {
                    selected = match
                } else {
                    selected = nil
                }
            }
        )) {
            TableColumn("Time") { event in
                Text(timestampFormatter.string(from: event.timestamp))
                    .font(.system(.caption, design: .monospaced))
            }
            .width(min: 110, ideal: 120)
            TableColumn("Stage") { event in
                Text(event.stage)
                    .font(.system(.caption, design: .monospaced))
            }
            .width(min: 150, ideal: 170)
            TableColumn("Outcome") { event in
                Text(event.outcome)
                    .font(.caption.weight(.semibold))
                    .foregroundStyle(event.outcomeColor)
            }
            .width(min: 70, ideal: 80)
            TableColumn("Work Item") { event in
                Text(event.workItemId.map(DispatchEvent.shortId) ?? "—")
                    .font(.system(.caption, design: .monospaced))
            }
            .width(min: 90, ideal: 110)
            TableColumn("Execution") { event in
                Text(DispatchEvent.shortId(event.executionId))
                    .font(.system(.caption, design: .monospaced))
            }
            .width(min: 90, ideal: 110)
            TableColumn("Worker") { event in
                Text(event.workerId ?? "—")
                    .font(.system(.caption, design: .monospaced))
            }
            .width(min: 70, ideal: 90)
            TableColumn("Error") { event in
                Text(event.errorMessage ?? "")
                    .lineLimit(1)
                    .truncationMode(.tail)
                    .foregroundStyle(event.errorMessage == nil ? .secondary : .primary)
                    .font(.caption)
            }
            .width(min: 140)
        }
    }
}

private struct FilterChip: View {
    let label: String
    let active: Bool
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            Text(label)
                .font(.system(.caption, design: .monospaced))
                .padding(.horizontal, 8)
                .padding(.vertical, 3)
                .background(
                    RoundedRectangle(cornerRadius: 6, style: .continuous)
                        .fill(active ? Color.accentColor.opacity(0.25) : Color.gray.opacity(0.12))
                )
                .overlay(
                    RoundedRectangle(cornerRadius: 6, style: .continuous)
                        .stroke(active ? Color.accentColor : Color.gray.opacity(0.3), lineWidth: 1)
                )
        }
        .buttonStyle(.plain)
    }
}

private struct DispatchExecutionDrawer: View {
    let event: DispatchEvent
    let onClose: () -> Void

    @State private var timeline: DispatchExecutionTimeline?

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack {
                VStack(alignment: .leading, spacing: 2) {
                    Text("Execution timeline")
                        .font(.headline)
                    Text(event.executionId)
                        .font(.system(.caption, design: .monospaced))
                        .textSelection(.enabled)
                }
                Spacer()
                Button(action: revealInFinder) {
                    Image(systemName: "folder")
                }
                .buttonStyle(.borderless)
                .help("Reveal per-execution dispatch.jsonl in Finder")
                Button(action: onClose) {
                    Image(systemName: "xmark.circle.fill")
                }
                .buttonStyle(.borderless)
            }
            Divider()
            ScrollView {
                if let timeline {
                    if timeline.missing {
                        Text("No per-execution mirror at \(DispatchEventsPaths.executionJsonl(executionId: event.executionId).path)")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                            .padding(.vertical, 8)
                    } else {
                        VStack(alignment: .leading, spacing: 10) {
                            ForEach(Array(timeline.events.enumerated()), id: \.offset) { idx, event in
                                DispatchTimelineRow(
                                    event: event,
                                    previous: idx > 0 ? timeline.events[idx - 1] : nil
                                )
                            }
                        }
                        .padding(.vertical, 4)
                    }
                } else {
                    ProgressView()
                        .padding()
                }
            }
        }
        .padding(10)
        .onAppear(perform: load)
        .onChange(of: event.executionId, initial: false) { _, _ in
            timeline = nil
            load()
        }
    }

    private func load() {
        let id = event.executionId
        DispatchQueue.global(qos: .utility).async {
            let loaded = DispatchExecutionLoader.load(executionId: id)
            DispatchQueue.main.async {
                self.timeline = loaded
            }
        }
    }

    private func revealInFinder() {
        let url = DispatchEventsPaths.executionJsonl(executionId: event.executionId)
        NSWorkspace.shared.activateFileViewerSelecting([url])
    }
}

private struct DispatchTimelineRow: View {
    let event: DispatchEvent
    let previous: DispatchEvent?

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack(spacing: 6) {
                Text(event.stage)
                    .font(.system(.caption, design: .monospaced).weight(.semibold))
                Text(event.outcome)
                    .font(.caption.weight(.semibold))
                    .foregroundStyle(event.outcomeColor)
                Spacer()
                Text(timestampFormatter.string(from: event.timestamp))
                    .font(.system(.caption, design: .monospaced))
                    .foregroundStyle(.secondary)
                if let previous {
                    let delta = Double(event.tsEpochMs - previous.tsEpochMs) / 1000.0
                    Text(String(format: "+%.2fs", delta))
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                }
            }
            if let err = event.errorMessage {
                Text(err)
                    .font(.system(.caption, design: .monospaced))
                    .foregroundStyle(.red)
                    .textSelection(.enabled)
            }
            if let details = event.detailsJSON, details != "null" {
                Text(details)
                    .font(.system(.caption2, design: .monospaced))
                    .foregroundStyle(.secondary)
                    .textSelection(.enabled)
            }
            HStack(spacing: 6) {
                Button("Copy as JSON") {
                    let pb = NSPasteboard.general
                    pb.clearContents()
                    pb.setString(asJSON(event), forType: .string)
                }
                .buttonStyle(.borderless)
                .font(.caption2)
            }
        }
        .padding(8)
        .background(
            RoundedRectangle(cornerRadius: 6, style: .continuous)
                .fill(Color.gray.opacity(0.06))
        )
    }

    private func asJSON(_ event: DispatchEvent) -> String {
        var dict: [String: Any] = [
            "ts_epoch_ms": event.tsEpochMs,
            "stage": event.stage,
            "outcome": event.outcome,
            "execution_id": event.executionId,
        ]
        if let v = event.workItemId { dict["work_item_id"] = v }
        if let v = event.workerId { dict["worker_id"] = v }
        if let v = event.cubeRepoId { dict["cube_repo_id"] = v }
        if let v = event.cubeLeaseId { dict["cube_lease_id"] = v }
        if let v = event.cubeWorkspaceId { dict["cube_workspace_id"] = v }
        if let v = event.errorMessage { dict["error_message"] = v }
        guard let data = try? JSONSerialization.data(
            withJSONObject: dict,
            options: [.prettyPrinted, .sortedKeys]
        ), let s = String(data: data, encoding: .utf8) else {
            return ""
        }
        return s
    }
}

// Local timestamp formatter — `HH:mm:ss.SSS` per the chore spec.
private let timestampFormatter: DateFormatter = {
    let f = DateFormatter()
    f.locale = Locale(identifier: "en_US_POSIX")
    f.dateFormat = "HH:mm:ss.SSS"
    return f
}()

// MARK: - Window controller

@MainActor
final class DispatchEventsWindowController: NSObject, NSWindowDelegate {
    static let shared = DispatchEventsWindowController()

    static let visibilityDefaultsKey = "boss.dispatchEventsViewer.visible"
    static let frameDefaultsKey = "boss.dispatchEventsViewer.frame"

    private var window: NSWindow?
    private var modelHost: NSHostingView<DispatchEventsViewer>?

    private override init() {
        super.init()
    }

    var isVisible: Bool {
        window?.isVisible ?? false
    }

    func toggle() {
        if isVisible {
            hide()
        } else {
            show()
        }
    }

    /// Restore visibility on app launch — call after the app's main
    /// window has been installed.
    func restoreIfNeeded() {
        if UserDefaults.standard.bool(forKey: Self.visibilityDefaultsKey) {
            show(activate: false)
        }
    }

    func show(activate: Bool = true) {
        let window = ensureWindow()
        window.makeKeyAndOrderFront(nil)
        if activate {
            NSApp.activate(ignoringOtherApps: true)
        }
        UserDefaults.standard.set(true, forKey: Self.visibilityDefaultsKey)
    }

    func hide() {
        window?.orderOut(nil)
        UserDefaults.standard.set(false, forKey: Self.visibilityDefaultsKey)
    }

    private func ensureWindow() -> NSWindow {
        if let window { return window }
        let host = NSHostingView(rootView: DispatchEventsViewer())
        let frame = persistedFrame() ?? NSRect(x: 0, y: 0, width: 1040, height: 620)
        let window = NSWindow(
            contentRect: frame,
            styleMask: [.titled, .closable, .miniaturizable, .resizable],
            backing: .buffered,
            defer: false
        )
        window.title = "Dispatch Events"
        window.contentView = host
        window.isReleasedWhenClosed = false
        window.delegate = self
        window.setFrameAutosaveName("BossDispatchEventsViewer")
        if persistedFrame() == nil {
            window.center()
        }
        self.window = window
        self.modelHost = host
        return window
    }

    private func persistedFrame() -> NSRect? {
        guard let s = UserDefaults.standard.string(forKey: Self.frameDefaultsKey) else {
            return nil
        }
        let r = NSRectFromString(s)
        return r == .zero ? nil : r
    }

    // MARK: NSWindowDelegate

    func windowWillClose(_ notification: Notification) {
        guard let window = notification.object as? NSWindow else { return }
        UserDefaults.standard.set(NSStringFromRect(window.frame), forKey: Self.frameDefaultsKey)
        UserDefaults.standard.set(false, forKey: Self.visibilityDefaultsKey)
    }

    func windowDidMove(_ notification: Notification) {
        guard let window = notification.object as? NSWindow else { return }
        UserDefaults.standard.set(NSStringFromRect(window.frame), forKey: Self.frameDefaultsKey)
    }

    func windowDidResize(_ notification: Notification) {
        guard let window = notification.object as? NSWindow else { return }
        UserDefaults.standard.set(NSStringFromRect(window.frame), forKey: Self.frameDefaultsKey)
    }
}
