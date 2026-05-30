import SwiftUI

// MARK: - ExecutionRow

/// Renders one row of the transcript viewer's execution list.
struct ExecutionRow: View {
    let exec: ExecutionVM

    var body: some View {
        VStack(alignment: .leading, spacing: 3) {
            HStack(spacing: 6) {
                Text(kindLabel)
                    .font(.headline)
                    .lineLimit(1)
                Spacer()
                statusBadge
            }
            if let model = exec.model, !model.isEmpty {
                Text(model)
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            if let start = exec.startedAt {
                Text(formattedDate(start))
                    .font(.caption2)
                    .foregroundStyle(.tertiary)
            }
        }
        .padding(.vertical, 2)
    }

    private var kindLabel: String {
        switch exec.kind {
        case "project_design": "Project Design"
        case "revision_implementation": "Revision"
        case "ci_remediation": "CI Remediation"
        case "investigation": "Investigation"
        default: exec.kind.replacingOccurrences(of: "_", with: " ").capitalized
        }
    }

    private var statusBadge: some View {
        Text(exec.status.capitalized)
            .font(.caption2)
            .padding(.horizontal, 6)
            .padding(.vertical, 2)
            .background(statusColor.opacity(0.15))
            .foregroundStyle(statusColor)
            .clipShape(Capsule())
    }

    private var statusColor: Color {
        switch exec.status {
        case "succeeded": .green
        case "running": .blue
        case "failed": .red
        case "abandoned": .orange
        case "pending": .yellow
        default: .secondary
        }
    }

    private func formattedDate(_ iso: String) -> String {
        let formatter = ISO8601DateFormatter()
        formatter.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        if let date = formatter.date(from: iso) {
            let display = DateFormatter()
            display.dateStyle = .short
            display.timeStyle = .short
            return display.string(from: date)
        }
        // Fall back without fractional seconds
        let formatter2 = ISO8601DateFormatter()
        if let date = formatter2.date(from: iso) {
            let display = DateFormatter()
            display.dateStyle = .short
            display.timeStyle = .short
            return display.string(from: date)
        }
        return iso
    }
}

// MARK: - TranscriptViewerView

/// Master/detail transcript viewer: execution list on the left, transcript
/// placeholder on the right. The lazy segment renderer is task 4; this task
/// delivers the window skeleton, EngineClient wiring, and execution list.
struct TranscriptViewerView: View {
    let ref: TranscriptViewerRef

    @EnvironmentObject private var chatModel: ChatViewModel
    @State private var selectedExecutionId: String?

    private var executions: [ExecutionVM] {
        chatModel.executionsByTaskID[ref.taskId] ?? []
    }

    private var isLoading: Bool {
        chatModel.executionsByTaskID[ref.taskId] == nil
    }

    var body: some View {
        NavigationSplitView {
            executionList
                .navigationSplitViewColumnWidth(min: 220, ideal: 260, max: 320)
        } detail: {
            transcriptDetail
        }
        .onAppear {
            chatModel.loadExecutions(taskId: ref.taskId)
            if let preselect = ref.preselectExecutionId {
                selectedExecutionId = preselect
                chatModel.loadTranscript(executionId: preselect)
            }
        }
        .onChange(of: selectedExecutionId) { _, newValue in
            if let execId = newValue {
                chatModel.loadTranscript(executionId: execId)
            }
        }
        .navigationTitle("Transcripts")
    }

    // MARK: Left pane — execution list

    @ViewBuilder
    private var executionList: some View {
        Group {
            if isLoading {
                VStack {
                    Spacer()
                    ProgressView()
                    Spacer()
                }
            } else if executions.isEmpty {
                ContentUnavailableView(
                    "No Executions",
                    systemImage: "clock.arrow.trianglehead.counterclockwise.rotate.90",
                    description: Text("This task has not been run yet.")
                )
            } else {
                List(executions, selection: $selectedExecutionId) { exec in
                    ExecutionRow(exec: exec)
                        .tag(exec.id)
                }
            }
        }
        .navigationTitle("Executions")
    }

    // MARK: Right pane — transcript

    @ViewBuilder
    private var transcriptDetail: some View {
        if let execId = selectedExecutionId {
            switch chatModel.transcriptsByExecutionID[execId] {
            case .loaded(let doc):
                TranscriptView(
                    doc: doc,
                    onRefresh: { chatModel.refreshTranscript(executionId: execId) }
                )
                .navigationTitle(transcriptTitle(for: execId))
                .task(id: doc.isLive) {
                    guard doc.isLive else { return }
                    while !Task.isCancelled {
                        try? await Task.sleep(for: .seconds(5))
                        guard !Task.isCancelled else { return }
                        chatModel.refreshTranscript(executionId: execId)
                    }
                }
            case .unavailable(let reason):
                ContentUnavailableView {
                    Label("Transcript Unavailable", systemImage: "doc.questionmark")
                } description: {
                    Text(reason)
                }
            case .loading, .none:
                VStack {
                    Spacer()
                    ProgressView()
                    Spacer()
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            }
        } else {
            ContentUnavailableView(
                "No Execution Selected",
                systemImage: "text.bubble",
                description: Text("Select an execution from the list to view its transcript.")
            )
        }
    }

    private func transcriptTitle(for execId: String) -> String {
        guard let exec = executions.first(where: { $0.id == execId }) else {
            return "Transcript"
        }
        return exec.kind.replacingOccurrences(of: "_", with: " ").capitalized
    }
}
