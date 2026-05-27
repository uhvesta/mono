import AppKit
import Foundation
import SwiftUI

// MARK: - Activity types

enum ActivitySourceFilter: String, CaseIterable, Identifiable {
    case all = "All"
    case dispatch = "Dispatch"
    case engineAttempts = "Engine"
    var id: String { rawValue }
}

enum ActivityOutcome: String, CaseIterable, Identifiable {
    case success
    case error
    case skipped
    case pending
    case abandoned

    var id: String { rawValue }

    var color: Color {
        switch self {
        case .success: return .secondary
        case .error: return .red
        case .skipped: return .orange
        case .pending: return .blue
        case .abandoned: return .secondary
        }
    }

    var badgeBackground: Color {
        switch self {
        case .success: return Color.secondary.opacity(0.12)
        case .error: return Color.red.opacity(0.18)
        case .skipped: return Color.orange.opacity(0.18)
        case .pending: return Color.blue.opacity(0.18)
        case .abandoned: return Color.gray.opacity(0.18)
        }
    }
}

// MARK: - ActivityRow

struct ActivityRow: Identifiable, Hashable {
    enum Payload: Hashable {
        case dispatch(DispatchEvent)
        case engineAttempt(EngineAttemptRow)
    }

    let id: String
    let timestamp: Date
    let payload: Payload

    var sourceLabel: String {
        switch payload {
        case .dispatch: return "dispatch"
        case .engineAttempt: return "engine"
        }
    }

    var headline: String {
        switch payload {
        case .dispatch(let e): return e.stage
        case .engineAttempt(let r): return "\(r.kindLabel): \(prShortNumber(r.prURL))"
        }
    }

    var outcome: ActivityOutcome {
        switch payload {
        case .dispatch(let e):
            switch e.outcome {
            case "ok": return .success
            case "error": return .error
            case "skipped": return .skipped
            default: return .success
            }
        case .engineAttempt(let r):
            switch r.status {
            case "succeeded": return .success
            case "failed": return .error
            case "abandoned", "superseded": return .abandoned
            default: return .pending
            }
        }
    }

    var workItemId: String? {
        switch payload {
        case .dispatch(let e): return e.workItemId
        case .engineAttempt(let r): return r.workItemID
        }
    }

    var entityId: String {
        switch payload {
        case .dispatch(let e): return DispatchEvent.shortId(e.executionId)
        case .engineAttempt(let r): return String(r.id.suffix(8))
        }
    }

    var searchHaystack: String {
        switch payload {
        case .dispatch(let e):
            return [e.executionId, e.workItemId ?? "", e.workerId ?? "",
                    e.errorMessage ?? "", e.stage].joined(separator: " ")
        case .engineAttempt(let r):
            return [r.id, r.workItemID, r.prURL, r.failureReason ?? ""].joined(separator: " ")
        }
    }

    private func prShortNumber(_ url: String) -> String {
        guard let range = url.range(of: "/pull/") else { return url }
        return "#" + url[range.upperBound...]
    }
}

// MARK: - ActivityLogModel

@MainActor
final class ActivityLogModel: ObservableObject {
    @Published var dispatchEvents: [DispatchEvent] = []
    @Published var dispatchSourceMissing: Bool = false
    @Published var lastDispatchEventAt: Date?

    private var tailer: DispatchEventsTailer?
    private let isoFormatter = ISO8601DateFormatter()

    func start() {
        guard tailer == nil else { return }
        let t = DispatchEventsTailer(
            url: DispatchEventsPaths.currentJsonl(),
            onEvents: { [weak self] new in
                self?.appendDispatch(new)
            },
            onEmpty: { [weak self] in
                self?.dispatchSourceMissing = true
            }
        )
        tailer = t
        t.start()
    }

    func stop() {
        tailer?.stop()
        tailer = nil
    }

    private func appendDispatch(_ new: [DispatchEvent]) {
        dispatchSourceMissing = false
        dispatchEvents.insert(contentsOf: new.reversed(), at: 0)
        if dispatchEvents.count > 5_000 {
            dispatchEvents.removeLast(dispatchEvents.count - 5_000)
        }
        lastDispatchEventAt = new.last.map { $0.timestamp } ?? lastDispatchEventAt
    }

    /// Build the merged, newest-first activity list. Engine attempts
    /// are *derived* from the engine's `conflict_resolutions` +
    /// `ci_remediations` snapshots (owned by `ChatViewModel`) rather
    /// than mirrored into a `@Published` here — the caller threads the
    /// current snapshots straight through. Keeping the merge a pure
    /// function of upstream state means it can never publish a change
    /// from within a view update. Design Phase 11 #37 grew this from
    /// one row kind to two; future kinds (rebase, review-feedback) just
    /// add another parameter and `map`.
    func makeRows(
        sourceFilter: ActivitySourceFilter,
        conflicts: [WorkConflictResolution] = [],
        ci: [WorkCiRemediation] = []
    ) -> [ActivityRow] {
        let d: [ActivityRow] = dispatchEvents.map { e in
            ActivityRow(id: "d:\(e.id)", timestamp: e.timestamp, payload: .dispatch(e))
        }
        let engineAttempts = conflicts.map { EngineAttemptRow.conflictResolution($0) }
            + ci.map { EngineAttemptRow.ciRemediation($0) }
        let ea: [ActivityRow] = engineAttempts.map { r in
            ActivityRow(
                id: "e:\(r.id)",
                timestamp: isoFormatter.date(from: r.createdAt) ?? .distantPast,
                payload: .engineAttempt(r)
            )
        }
        let combined: [ActivityRow]
        switch sourceFilter {
        case .all: combined = d + ea
        case .dispatch: combined = d
        case .engineAttempts: combined = ea
        }
        return combined.sorted { $0.timestamp > $1.timestamp }
    }
}

// MARK: - ActivityLogView

struct ActivityLogView: View {
    @EnvironmentObject private var chat: ChatViewModel
    @StateObject private var model = ActivityLogModel()

    @AppStorage("boss.activityLog.sourceFilter") private var sourceFilterRaw =
        ActivitySourceFilter.all.rawValue
    @AppStorage("boss.activityLog.outcomeFilter") private var outcomeFilterRaw = ""
    @AppStorage("boss.activityLog.searchText") private var searchText = ""

    @State private var selectedRowId: String?

    private var sourceFilter: ActivitySourceFilter {
        ActivitySourceFilter(rawValue: sourceFilterRaw) ?? .all
    }

    private var outcomeFilter: Set<ActivityOutcome> {
        Set(
            outcomeFilterRaw
                .split(separator: ",")
                .compactMap { ActivityOutcome(rawValue: String($0)) }
        )
    }

    private var availableOutcomes: [ActivityOutcome] {
        switch sourceFilter {
        case .all: return ActivityOutcome.allCases
        case .dispatch: return [.success, .error, .skipped]
        case .engineAttempts: return [.success, .error, .pending, .abandoned]
        }
    }

    private var filteredRows: [ActivityRow] {
        let raw = model.makeRows(
            sourceFilter: sourceFilter,
            conflicts: chat.conflictResolutions,
            ci: chat.ciRemediations
        )
        let outcomes = outcomeFilter
        let query = searchText.lowercased().trimmingCharacters(in: .whitespacesAndNewlines)
        return raw.filter { row in
            if !outcomes.isEmpty, !outcomes.contains(row.outcome) { return false }
            if !query.isEmpty, !row.searchHaystack.lowercased().contains(query) { return false }
            return true
        }
    }

    var body: some View {
        VStack(spacing: 0) {
            headerBar
            Divider()
            filterBar
            Divider()
            mainContent
        }
        .onAppear {
            model.start()
        }
        .onDisappear {
            model.stop()
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Color(nsColor: .windowBackgroundColor))
    }

    // MARK: - Header

    private var headerBar: some View {
        HStack(spacing: 12) {
            Text("Activity Log")
                .font(.title3.bold())
            Spacer()
            VStack(alignment: .trailing, spacing: 2) {
                Text("\(model.dispatchEvents.count) dispatch • \(chat.conflictResolutions.count + chat.ciRemediations.count) engine")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                if let last = model.lastDispatchEventAt {
                    Text("last dispatch \(activityTimestampFormatter.string(from: last))")
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                }
            }
            Button {
                chat.refreshEngineAttempts()
            } label: {
                Label("Refresh", systemImage: "arrow.clockwise")
            }
            .help("Refresh engine attempts from engine")
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 10)
    }

    // MARK: - Filter bar

    private var filterBar: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 12) {
                HStack(spacing: 6) {
                    Text("Source").font(.caption).foregroundStyle(.secondary)
                    Picker("Source", selection: Binding(
                        get: { sourceFilter },
                        set: { sourceFilterRaw = $0.rawValue }
                    )) {
                        ForEach(ActivitySourceFilter.allCases) { f in
                            Text(f.rawValue).tag(f)
                        }
                    }
                    .pickerStyle(.segmented)
                    .frame(width: 200)
                }
                Spacer()
                TextField("search id / error", text: $searchText)
                    .textFieldStyle(.roundedBorder)
                    .frame(width: 240)
                Button("Clear") {
                    sourceFilterRaw = ActivitySourceFilter.all.rawValue
                    outcomeFilterRaw = ""
                    searchText = ""
                }
                .buttonStyle(.borderless)
                .disabled(sourceFilter == .all && outcomeFilter.isEmpty && searchText.isEmpty)
            }
            HStack(spacing: 6) {
                Text("Outcome").font(.caption).foregroundStyle(.secondary)
                ForEach(availableOutcomes) { outcome in
                    ActivityFilterChip(
                        label: outcome.rawValue,
                        active: outcomeFilter.contains(outcome),
                        action: { toggleOutcome(outcome) }
                    )
                }
            }
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 8)
    }

    // MARK: - Main content

    @ViewBuilder
    private var mainContent: some View {
        let rows = filteredRows
        if model.dispatchEvents.isEmpty && chat.conflictResolutions.isEmpty && chat.ciRemediations.isEmpty {
            emptyState
        } else {
            HSplitView {
                activityTable(rows: rows)
                    .frame(minWidth: 520)
                detailPane(rows: rows)
                    .frame(minWidth: 300, maxWidth: .infinity)
            }
        }
    }

    private var emptyState: some View {
        VStack(spacing: 10) {
            Text("No activity yet")
                .font(.headline)
            Text(
                "Dispatch events and engine attempts appear here as work is dispatched."
            )
            .multilineTextAlignment(.center)
            .foregroundStyle(.secondary)
            .frame(maxWidth: 480)
            if model.dispatchSourceMissing {
                Text(DispatchEventsPaths.currentJsonl().path)
                    .font(.system(.caption, design: .monospaced))
                    .foregroundStyle(.secondary)
                    .textSelection(.enabled)
            }
        }
        .padding()
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    private func activityTable(rows: [ActivityRow]) -> some View {
        Table(rows, selection: Binding(
            get: { selectedRowId },
            set: { selectedRowId = $0 }
        )) {
            TableColumn("Time") { row in
                Text(activityTimestampFormatter.string(from: row.timestamp))
                    .font(.system(.caption, design: .monospaced))
            }
            .width(min: 100, ideal: 110)

            TableColumn("Source") { row in
                Text(row.sourceLabel)
                    .font(.system(.caption, design: .monospaced))
                    .foregroundStyle(.secondary)
            }
            .width(min: 56, ideal: 70)

            TableColumn("Headline") { row in
                Text(row.headline)
                    .font(.system(.caption, design: .monospaced))
                    .lineLimit(1)
            }
            .width(min: 140, ideal: 200)

            TableColumn("Outcome") { row in
                ActivityOutcomeBadge(outcome: row.outcome)
            }
            .width(min: 75, ideal: 90)

            TableColumn("Work Item") { row in
                if let wi = row.workItemId {
                    Text(DispatchEvent.shortId(wi))
                        .font(.system(.caption, design: .monospaced))
                } else {
                    Text("—").foregroundStyle(.tertiary)
                }
            }
            .width(min: 60, ideal: 80)

            TableColumn("ID") { row in
                Text(row.entityId)
                    .font(.system(.caption, design: .monospaced))
                    .lineLimit(1)
                    .truncationMode(.middle)
            }
            .width(min: 56, ideal: 80)
        }
    }

    @ViewBuilder
    private func detailPane(rows: [ActivityRow]) -> some View {
        if let rowId = selectedRowId,
           let row = rows.first(where: { $0.id == rowId }) {
            switch row.payload {
            case .dispatch(let event):
                ActivityDispatchDetailPane(event: event) {
                    selectedRowId = nil
                }
            case .engineAttempt(let attempt):
                ActivityEngineDetailPane(row: attempt) {
                    selectedRowId = nil
                }
            }
        } else {
            VStack {
                Spacer()
                Text("Select a row to see details.")
                    .foregroundStyle(.secondary)
                Spacer()
            }
            .frame(maxWidth: .infinity)
        }
    }

    private func toggleOutcome(_ outcome: ActivityOutcome) {
        var current = outcomeFilter
        if current.contains(outcome) { current.remove(outcome) } else { current.insert(outcome) }
        outcomeFilterRaw = current.map(\.rawValue).sorted().joined(separator: ",")
    }
}

// MARK: - Dispatch detail pane

private struct ActivityDispatchDetailPane: View {
    let event: DispatchEvent
    let onClose: () -> Void

    @State private var timeline: ActivityDispatchTimeline?

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
                        Text(
                            "No per-execution mirror at \(DispatchEventsPaths.executionJsonl(executionId: event.executionId).path)"
                        )
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .padding(.vertical, 8)
                    } else {
                        VStack(alignment: .leading, spacing: 10) {
                            ForEach(Array(timeline.events.enumerated()), id: \.offset) { idx, evt in
                                ActivityDispatchTimelineRow(
                                    event: evt,
                                    previous: idx > 0 ? timeline.events[idx - 1] : nil
                                )
                            }
                        }
                        .padding(.vertical, 4)
                    }
                } else {
                    ProgressView().padding()
                }
            }
        }
        .padding(10)
        .onAppear { load() }
        .onChange(of: event.executionId, initial: false) { _, _ in
            timeline = nil
            load()
        }
    }

    private func load() {
        let id = event.executionId
        DispatchQueue.global(qos: .utility).async {
            let loaded = ActivityDispatchLoader.load(executionId: id)
            DispatchQueue.main.async { self.timeline = loaded }
        }
    }

    private func revealInFinder() {
        let url = DispatchEventsPaths.executionJsonl(executionId: event.executionId)
        NSWorkspace.shared.activateFileViewerSelecting([url])
    }
}

private struct ActivityDispatchTimeline {
    let executionId: String
    let events: [DispatchEvent]
    let missing: Bool
}

private enum ActivityDispatchLoader {
    static func load(executionId: String) -> ActivityDispatchTimeline {
        let url = DispatchEventsPaths.executionJsonl(executionId: executionId)
        guard let data = try? Data(contentsOf: url),
              let text = String(data: data, encoding: .utf8) else {
            return ActivityDispatchTimeline(executionId: executionId, events: [], missing: true)
        }
        var parsed: [DispatchEvent] = []
        for line in text.split(separator: "\n", omittingEmptySubsequences: true) {
            if let event = DispatchEventDecoder.decode(line: String(line)) {
                parsed.append(event)
            }
        }
        parsed.sort { $0.tsEpochMs < $1.tsEpochMs }
        return ActivityDispatchTimeline(executionId: executionId, events: parsed, missing: false)
    }
}

private struct ActivityDispatchTimelineRow: View {
    let event: DispatchEvent
    let previous: DispatchEvent?

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack(spacing: 6) {
                Text(event.stage)
                    .font(.system(.caption, design: .monospaced).weight(.semibold))
                outcomeLabel
                Spacer()
                Text(activityTimestampFormatter.string(from: event.timestamp))
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
            if let cmd = event.cubeCommand {
                HStack(spacing: 4) {
                    Text(cmd)
                        .font(.system(.caption, design: .monospaced))
                        .textSelection(.enabled)
                    Button {
                        let pb = NSPasteboard.general
                        pb.clearContents()
                        pb.setString(cmd, forType: .string)
                    } label: {
                        Image(systemName: "doc.on.doc")
                    }
                    .buttonStyle(.borderless)
                    .font(.caption2)
                    .help("Copy command to clipboard")
                }
            }
            if let cwd = event.cubeCwd {
                Text("cwd: \(cwd)")
                    .font(.system(.caption2, design: .monospaced))
                    .foregroundStyle(.secondary)
                    .textSelection(.enabled)
            }
            if let details = event.detailsJSON, details != "null" {
                Text(details)
                    .font(.system(.caption2, design: .monospaced))
                    .foregroundStyle(.secondary)
                    .textSelection(.enabled)
            }
            Button("Copy as JSON") {
                let pb = NSPasteboard.general
                pb.clearContents()
                pb.setString(eventAsJSON(event), forType: .string)
            }
            .buttonStyle(.borderless)
            .font(.caption2)
        }
        .padding(8)
        .background(
            RoundedRectangle(cornerRadius: 6, style: .continuous)
                .fill(Color.gray.opacity(0.06))
        )
    }

    private var outcomeLabel: some View {
        let outcome: ActivityOutcome = {
            switch event.outcome {
            case "ok": return .success
            case "error": return .error
            case "skipped": return .skipped
            default: return .success
            }
        }()
        return Text(event.outcome)
            .font(.caption.weight(.semibold))
            .foregroundStyle(outcome.color)
    }

    private func eventAsJSON(_ event: DispatchEvent) -> String {
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
        if let v = event.cubeCommand { dict["cube_command"] = v }
        if let v = event.cubeCwd { dict["cube_cwd"] = v }
        guard let data = try? JSONSerialization.data(
            withJSONObject: dict, options: [.prettyPrinted, .sortedKeys]
        ), let s = String(data: data, encoding: .utf8) else { return "" }
        return s
    }
}

// MARK: - Engine attempt detail pane

private struct ActivityEngineDetailPane: View {
    let row: EngineAttemptRow
    let onClose: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack {
                Text("\(row.kindLabel) attempt")
                    .font(.headline)
                Spacer()
                Button(action: onClose) {
                    Image(systemName: "xmark.circle.fill")
                }
                .buttonStyle(.borderless)
            }
            .padding(.horizontal, 10)
            .padding(.vertical, 8)
            Divider()
            ScrollView {
                VStack(alignment: .leading, spacing: 10) {
                    Group {
                        engineDetailRow("Attempt id", body: row.id)
                        engineDetailRow("Status", body: row.status)
                        engineDetailRow("PR", body: row.prURL)
                        engineDetailRow("Work item", body: row.workItemID)
                        engineDetailRow("Created", body: row.createdAt)
                        engineDetailRow("Finished", body: row.finishedAt ?? "—")
                        if let reason = row.failureReason, !reason.isEmpty {
                            engineDetailRow("Failure reason", body: reason)
                        }
                    }
                    switch row {
                    case .conflictResolution(let resolution):
                        if let diag = resolution.conflictDiagnosis, !diag.isEmpty {
                            Text("Diagnosis")
                                .font(.subheadline.bold())
                                .padding(.top, 8)
                            ScrollView {
                                Text(diag)
                                    .font(.system(.caption, design: .monospaced))
                                    .textSelection(.enabled)
                                    .frame(maxWidth: .infinity, alignment: .leading)
                            }
                            .frame(maxHeight: 220)
                            .background(Color(nsColor: .textBackgroundColor))
                            .clipShape(RoundedRectangle(cornerRadius: 6))
                        }
                    case .ciRemediation(let remediation):
                        engineDetailRow("Attempt kind", body: remediation.attemptKind)
                        engineDetailRow(
                            "Consumes budget",
                            body: remediation.consumesBudget == 1 ? "yes" : "no"
                        )
                        engineDetailRow(
                            "Triage class",
                            body: remediation.triageClass ?? "—"
                        )
                        engineDetailRow("Head SHA", body: remediation.headSHAAtTrigger)
                        if !remediation.failedChecks.isEmpty,
                           remediation.failedChecks != "[]" {
                            Text("Failed checks")
                                .font(.subheadline.bold())
                                .padding(.top, 8)
                            ScrollView {
                                Text(remediation.failedChecks)
                                    .font(.system(.caption, design: .monospaced))
                                    .textSelection(.enabled)
                                    .frame(maxWidth: .infinity, alignment: .leading)
                            }
                            .frame(maxHeight: 180)
                            .background(Color(nsColor: .textBackgroundColor))
                            .clipShape(RoundedRectangle(cornerRadius: 6))
                        }
                        if let log = remediation.logExcerpt, !log.isEmpty {
                            Text("Log excerpt")
                                .font(.subheadline.bold())
                                .padding(.top, 8)
                            ScrollView {
                                Text(log)
                                    .font(.system(.caption, design: .monospaced))
                                    .textSelection(.enabled)
                                    .frame(maxWidth: .infinity, alignment: .leading)
                            }
                            .frame(maxHeight: 220)
                            .background(Color(nsColor: .textBackgroundColor))
                            .clipShape(RoundedRectangle(cornerRadius: 6))
                        }
                    }
                }
                .padding(16)
                .frame(maxWidth: .infinity, alignment: .topLeading)
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
    }

    @ViewBuilder
    private func engineDetailRow(_ label: String, body: String) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: 8) {
            Text(label + ":")
                .font(.caption.bold())
                .foregroundStyle(.secondary)
                .frame(width: 110, alignment: .trailing)
            Text(body)
                .font(.system(.body, design: .monospaced))
                .textSelection(.enabled)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
    }
}

// MARK: - Shared small views

private struct ActivityFilterChip: View {
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
                        .stroke(
                            active ? Color.accentColor : Color.gray.opacity(0.3),
                            lineWidth: 1
                        )
                )
        }
        .buttonStyle(.plain)
    }
}

private struct ActivityOutcomeBadge: View {
    let outcome: ActivityOutcome

    var body: some View {
        Text(outcome.rawValue)
            .font(.caption.bold())
            .padding(.horizontal, 7)
            .padding(.vertical, 2)
            .background(outcome.badgeBackground, in: Capsule(style: .continuous))
            .foregroundStyle(outcome.color)
    }
}

// MARK: - Formatters

private let activityTimestampFormatter: DateFormatter = {
    let f = DateFormatter()
    f.locale = Locale(identifier: "en_US_POSIX")
    f.dateFormat = "HH:mm:ss.SSS"
    return f
}()
