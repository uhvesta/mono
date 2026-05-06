import AppKit
import SwiftUI

struct WorkersDetailView: View {
    @ObservedObject var workspace: WorkersWorkspaceModel

    var body: some View {
        WorkerGrid(runtime: workspace.runtime, slots: workspace.slots)
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            .background(Color(nsColor: .separatorColor))
    }
}

private struct WorkerGrid: View {
    let runtime: GhosttyRuntime
    let slots: [WorkerSlot]

    var body: some View {
        let columns = 4
        let rows = stride(from: 0, to: slots.count, by: columns).map { start in
            Array(slots[start..<min(start + columns, slots.count)])
        }

        VStack(spacing: 1) {
            ForEach(Array(rows.enumerated()), id: \.offset) { _, row in
                HStack(spacing: 1) {
                    ForEach(row) { slot in
                        WorkerSlotView(runtime: runtime, slot: slot)
                            .frame(maxWidth: .infinity, maxHeight: .infinity)
                    }
                }
            }
        }
    }
}

private struct WorkerSlotView: View {
    let runtime: GhosttyRuntime
    let slot: WorkerSlot

    var body: some View {
        VStack(spacing: 0) {
            slotHeader
            Divider()
            slotBody
        }
        .background(Color(nsColor: .windowBackgroundColor))
    }

    @ViewBuilder
    private var slotBody: some View {
        if let session = slot.session {
            WorkerPaneTerminalView(runtime: runtime, session: session)
        } else {
            VStack(spacing: 4) {
                Spacer()
                Text("Slot \(slot.slotId)")
                    .font(.caption2)
                    .foregroundStyle(Color.white.opacity(0.45))
                Text("Free")
                    .font(.caption.weight(.medium))
                    .foregroundStyle(Color.white.opacity(0.7))
                Spacer()
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            .background(Color(nsColor: .black))
        }
    }

    private var slotHeader: some View {
        HStack(spacing: 8) {
            VStack(alignment: .leading, spacing: 1) {
                Text(WorkerNames.name(forSlot: slot.slotId))
                    .font(.caption.weight(.semibold))
                    .foregroundStyle(.secondary)
                    .lineLimit(1)

                slotSubtitle
            }

            Spacer(minLength: 0)

            if let state = slot.session?.claudeState {
                statusPill(state.label, color: claudeStateColor(state))
            }
        }
        .padding(.horizontal, 8)
        .padding(.vertical, 6)
        .help(slotTooltip)
    }

    private var slotTooltip: String {
        let base = "Worker \(slot.slotId)"
        if let runId = slot.runId {
            return "\(base) · run \(runId)"
        }
        return "\(base) · idle"
    }

    /// Second line in the titlebar. Prefers the engine-supplied
    /// human-readable summary; falls back to the run id when no
    /// summary is available so we never lose traceability. Either
    /// way we attach the run id as a hover tooltip — the summary is
    /// purely visual and the full id is what every log/api/taxonomy
    /// elsewhere uses.
    @ViewBuilder
    private var slotSubtitle: some View {
        if let summary = slot.summary, !summary.isEmpty {
            Text(summary)
                .font(.caption2)
                .foregroundStyle(.secondary)
                .lineLimit(1)
                .help(slot.runId ?? "")
        } else if let runId = slot.runId {
            Text(runId)
                .font(.caption2)
                .foregroundStyle(.secondary)
                .lineLimit(1)
                .help(runId)
        } else {
            Text("idle")
                .font(.caption2)
                .foregroundStyle(.tertiary)
                .lineLimit(1)
        }
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

    private func claudeStateColor(_ state: ClaudeMonitorState) -> Color {
        switch state {
        case .working: .blue
        case .ready: .green
        case .notDetected: .secondary
        case .unavailable: .orange
        }
    }
}

private struct WorkerPaneTerminalView: View {
    let runtime: GhosttyRuntime
    @ObservedObject var session: TerminalPaneSession

    var body: some View {
        GhosttyTerminalView(
            runtime: runtime,
            session: session,
            launchSpec: session.launchSpec
        )
        .background(Color(nsColor: .black))
    }
}
