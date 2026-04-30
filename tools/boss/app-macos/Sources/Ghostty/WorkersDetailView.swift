import AppKit
import SwiftUI

struct WorkersDetailView: View {
    @ObservedObject var workspace: WorkersWorkspaceModel

    var body: some View {
        WorkerGrid(runtime: workspace.runtime, panes: workspace.workerPanes)
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            .background(Color(nsColor: .separatorColor))
    }
}

private struct WorkerGrid: View {
    let runtime: GhosttyRuntime
    let panes: [TerminalPaneSession]

    var body: some View {
        let columns = 4
        let rows = stride(from: 0, to: panes.count, by: columns).map { start in
            Array(panes[start..<min(start + columns, panes.count)])
        }

        VStack(spacing: 1) {
            ForEach(Array(rows.enumerated()), id: \.offset) { _, row in
                HStack(spacing: 1) {
                    ForEach(row) { pane in
                        WorkerPaneView(runtime: runtime, session: pane)
                            .frame(maxWidth: .infinity, maxHeight: .infinity)
                    }
                }
            }
        }
    }
}

private struct WorkerPaneView: View {
    let runtime: GhosttyRuntime
    @ObservedObject var session: TerminalPaneSession

    var body: some View {
        VStack(spacing: 0) {
            paneHeader
            Divider()
            GhosttyTerminalView(
                runtime: runtime,
                session: session,
                launchSpec: session.launchSpec
            )
            .background(Color(nsColor: .black))
        }
        .background(Color(nsColor: .windowBackgroundColor))
    }

    private var paneHeader: some View {
        HStack(spacing: 8) {
            VStack(alignment: .leading, spacing: 1) {
                Text(session.role.defaultTitle)
                    .font(.caption.weight(.semibold))
                    .foregroundStyle(.secondary)
                    .lineLimit(1)

                Text(session.displayTitle)
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }

            Spacer(minLength: 0)

            statusPill(session.claudeState.label, color: claudeStateColor)
        }
        .padding(.horizontal, 8)
        .padding(.vertical, 6)
    }

    private func statusPill(_ text: String, color: Color) -> some View {
        Text(text)
            .font(.caption2.weight(.medium))
            .lineLimit(1)
            .padding(.horizontal, 8)
            .padding(.vertical, 4)
            .background(color.opacity(0.14))
            .foregroundStyle(color)
            .clipShape(Capsule())
    }

    private var claudeStateColor: Color {
        switch session.claudeState {
        case .working: .blue
        case .ready: .green
        case .notDetected: .secondary
        case .unavailable: .orange
        }
    }
}
