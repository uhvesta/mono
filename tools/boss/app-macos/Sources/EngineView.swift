import AppKit
import Foundation
import SwiftUI

/// Phase 5 #14 of the merge-conflict design: a tab that lists the
/// engine's attempt-shaped remediation feed. Today's source is the
/// `conflict_resolutions` table; the row representation
/// [[EngineAttemptRow]] is a sum type so the auto-rebase-stacked-prs
/// flow's `rebase_attempts` (and, later, `ci_remediations`) can grow
/// into the same list without re-architecting the view.
///
/// The view subscribes to the same product topics the work tab does
/// (the engine broadcasts `ConflictResolution*` events on the parent
/// product topic), so the list refreshes on a push without polling.
/// The Refresh button is a one-shot escape hatch for the "did I miss
/// a push?" case.
struct EngineView: View {
    @ObservedObject var chat: ChatViewModel

    @State private var sortColumn: SortColumn = .createdAt
    @State private var sortDirection: SortDirection = .descending
    @State private var statusFilter: StatusFilter = .all
    @State private var selectedAttemptID: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            header
            Divider()
            if attemptRows.isEmpty {
                emptyState
            } else {
                tableAndDetail
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
        .background(Color(nsColor: .windowBackgroundColor))
    }

    // MARK: - Composition

    private var header: some View {
        HStack(spacing: 12) {
            Text("Engine attempts")
                .font(.title3.bold())
            Spacer()
            Picker("Status", selection: $statusFilter) {
                ForEach(StatusFilter.allCases) { filter in
                    Text(filter.label).tag(filter)
                }
            }
            .pickerStyle(.menu)
            .frame(maxWidth: 200)
            Button {
                chat.refreshConflictResolutions()
            } label: {
                Label("Refresh", systemImage: "arrow.clockwise")
            }
            .help("Refresh the attempt list")
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 10)
    }

    private var emptyState: some View {
        VStack(spacing: 10) {
            Spacer()
            Image(systemName: "wrench.and.screwdriver")
                .font(.system(size: 40))
                .foregroundStyle(.tertiary)
            Text(statusFilter == .all
                 ? "No engine attempts yet."
                 : "No \(statusFilter.label.lowercased()) attempts.")
                .font(.headline)
                .foregroundStyle(.secondary)
            Text("Conflict-resolution rows land here when the engine spawns a worker to clear a merge conflict on an in-review PR.")
                .font(.subheadline)
                .foregroundStyle(.tertiary)
                .multilineTextAlignment(.center)
                .frame(maxWidth: 480)
            Spacer()
        }
        .frame(maxWidth: .infinity)
    }

    private var tableAndDetail: some View {
        HSplitView {
            attemptTable
                .frame(minWidth: 480)
            detailPane
                .frame(minWidth: 320)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
    }

    private var attemptTable: some View {
        Table(attemptRows, selection: $selectedAttemptID, sortOrder: tableSortBinding) {
            TableColumn("Kind", value: \.kindLabel) { row in
                Text(row.kindLabel)
            }
            .width(min: 64, ideal: 80, max: 120)
            TableColumn("Status", value: \.status) { row in
                StatusChipView(status: row.status)
            }
            .width(min: 90, ideal: 110, max: 140)
            TableColumn("PR", value: \.prURL) { row in
                Text(prShortLabel(row.prURL))
                    .help(row.prURL)
                    .lineLimit(1)
                    .truncationMode(.middle)
            }
            .width(min: 120, ideal: 200)
            TableColumn("Work item", value: \.workItemID) { row in
                Text(row.workItemID)
                    .font(.system(.body, design: .monospaced))
                    .lineLimit(1)
                    .truncationMode(.tail)
            }
            .width(min: 100, ideal: 160)
            TableColumn("Created", value: \.createdAt) { row in
                Text(row.createdAt)
                    .font(.system(.body, design: .monospaced))
                    .lineLimit(1)
                    .truncationMode(.tail)
            }
            .width(min: 120, ideal: 180)
            TableColumn("Finished", value: \.finishedAtSortKey) { row in
                Text(row.finishedAt ?? "—")
                    .font(.system(.body, design: .monospaced))
                    .foregroundStyle(row.finishedAt == nil ? .tertiary : .primary)
                    .lineLimit(1)
                    .truncationMode(.tail)
            }
            .width(min: 120, ideal: 180)
        }
    }

    @ViewBuilder
    private var detailPane: some View {
        if let attemptID = selectedAttemptID,
           let row = attemptRows.first(where: { $0.id == attemptID }) {
            EngineAttemptDetailView(row: row)
        } else {
            VStack {
                Spacer()
                Text("Select an attempt to see details.")
                    .foregroundStyle(.secondary)
                Spacer()
            }
            .frame(maxWidth: .infinity)
        }
    }

    // MARK: - Data

    private var attemptRows: [EngineAttemptRow] {
        let base = chat.conflictResolutions.map { EngineAttemptRow.conflictResolution($0) }
        let filtered = base.filter { statusFilter.matches($0.status) }
        return filtered.sorted(by: sortComparator)
    }

    private var tableSortBinding: Binding<[KeyPathComparator<EngineAttemptRow>]> {
        Binding(
            get: { [] },
            set: { _ in }
        )
    }

    private func sortComparator(_ lhs: EngineAttemptRow, _ rhs: EngineAttemptRow) -> Bool {
        let ordered: Bool
        switch sortColumn {
        case .kind:
            ordered = lhs.kindLabel < rhs.kindLabel
        case .status:
            ordered = lhs.status < rhs.status
        case .pr:
            ordered = lhs.prURL < rhs.prURL
        case .workItem:
            ordered = lhs.workItemID < rhs.workItemID
        case .createdAt:
            ordered = lhs.createdAt < rhs.createdAt
        case .finishedAt:
            ordered = (lhs.finishedAt ?? "") < (rhs.finishedAt ?? "")
        }
        return sortDirection == .ascending ? ordered : !ordered
    }

    private func prShortLabel(_ url: String) -> String {
        guard let hashRange = url.range(of: "/pull/") else { return url }
        let suffix = url[hashRange.upperBound...]
        return "#" + suffix
    }

    // MARK: - Sub-types

    private enum SortColumn {
        case kind, status, pr, workItem, createdAt, finishedAt
    }

    private enum SortDirection {
        case ascending, descending
    }

    private enum StatusFilter: String, CaseIterable, Identifiable {
        case all
        case running
        case succeeded
        case failed
        case abandoned

        var id: String { rawValue }

        var label: String {
            switch self {
            case .all: return "All"
            case .running: return "Running"
            case .succeeded: return "Succeeded"
            case .failed: return "Failed"
            case .abandoned: return "Abandoned"
            }
        }

        func matches(_ status: String) -> Bool {
            switch self {
            case .all:
                return true
            case .running:
                return status == "pending" || status == "running"
            case .succeeded:
                return status == "succeeded"
            case .failed:
                return status == "failed"
            case .abandoned:
                return status == "abandoned" || status == "superseded"
            }
        }
    }
}

private struct StatusChipView: View {
    let status: String

    var body: some View {
        Text(status)
            .font(.caption.bold())
            .padding(.horizontal, 8)
            .padding(.vertical, 3)
            .background(background, in: Capsule(style: .continuous))
            .foregroundStyle(foreground)
    }

    private var background: Color {
        switch status {
        case "succeeded": return Color.green.opacity(0.20)
        case "failed": return Color.red.opacity(0.20)
        case "abandoned", "superseded": return Color.gray.opacity(0.20)
        case "running": return Color.blue.opacity(0.20)
        case "pending": return Color.yellow.opacity(0.20)
        default: return Color.secondary.opacity(0.15)
        }
    }

    private var foreground: Color {
        switch status {
        case "succeeded": return .green
        case "failed": return .red
        case "abandoned", "superseded": return .secondary
        case "running": return .blue
        case "pending": return .orange
        default: return .primary
        }
    }
}

private struct EngineAttemptDetailView: View {
    let row: EngineAttemptRow

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 10) {
                Text(row.kindLabel + " attempt")
                    .font(.headline)
                Group {
                    Self.detailRow("Attempt id", body: row.id)
                    Self.detailRow("Status", body: row.status)
                    Self.detailRow("PR", body: row.prURL)
                    Self.detailRow("Work item", body: row.workItemID)
                    Self.detailRow("Created", body: row.createdAt)
                    Self.detailRow("Finished", body: row.finishedAt ?? "—")
                    if let reason = row.failureReason, !reason.isEmpty {
                        Self.detailRow("Failure reason", body: reason)
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
                }
            }
            .padding(16)
            .frame(maxWidth: .infinity, alignment: .topLeading)
        }
    }

    @ViewBuilder
    private static func detailRow(_ label: String, body: String) -> some View {
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

private extension EngineAttemptRow {
    /// Sort-stable key for "Finished" — pending rows sort after dated
    /// rows in descending order so an active attempt isn't buried.
    var finishedAtSortKey: String { finishedAt ?? "9999" }
}
