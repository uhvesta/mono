import AppKit
import SwiftUI

struct WorkersDetailView: View {
    @ObservedObject var workspace: WorkersWorkspaceModel
    @ObservedObject var liveStates: LiveWorkerStateStore

    var body: some View {
        WorkerGrid(runtime: workspace.runtime, slots: workspace.slots, liveStates: liveStates)
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            .background(Color(nsColor: .separatorColor))
    }
}

private struct WorkerGrid: View {
    let runtime: GhosttyRuntime
    let slots: [WorkerSlot]
    @ObservedObject var liveStates: LiveWorkerStateStore

    var body: some View {
        let columns = 4
        let rows = stride(from: 0, to: slots.count, by: columns).map { start in
            Array(slots[start..<min(start + columns, slots.count)])
        }

        VStack(spacing: 1) {
            ForEach(Array(rows.enumerated()), id: \.offset) { _, row in
                HStack(spacing: 1) {
                    ForEach(row) { slot in
                        WorkerSlotView(
                            runtime: runtime,
                            slot: slot,
                            liveState: liveStates.bySlot[slot.slotId]
                        )
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
    let liveState: WorkerLiveState?

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
            WorkerPaneTerminalView(
                runtime: runtime,
                session: session,
                liveState: liveState
            )
        } else {
            idlePaneView
        }
    }

    /// Idle / free slot treatment: large character portrait + crew name
    /// + a stable in-character recreational flavor line. The line is
    /// keyed on `slot.idleFlavorCycle`, which the workspace model
    /// bumps when the slot re-enters idle, so within one idle bout
    /// the line never flickers.
    @ViewBuilder
    private var idlePaneView: some View {
        let character = TrekCharacter.forSlot(slot.slotId)
        VStack(spacing: 14) {
            Spacer()
            if let character {
                if let nsImage = TrekIconAssets.image(character, size: .large) {
                    Image(nsImage: nsImage)
                        .resizable()
                        .interpolation(.high)
                        .aspectRatio(contentMode: .fit)
                        .frame(maxWidth: 220, maxHeight: 240)
                        .opacity(0.85)
                }
                Text(character.displayName)
                    .font(.title3.weight(.semibold))
                    .foregroundStyle(Color.white.opacity(0.85))
                Text(TrekIdleFlavor.line(for: character, cycle: slot.idleFlavorCycle))
                    .font(.callout)
                    .foregroundStyle(Color.white.opacity(0.6))
                    .multilineTextAlignment(.center)
                    .padding(.horizontal, 24)
                    .lineLimit(3)
            } else {
                Text("Slot \(slot.slotId)")
                    .font(.caption2)
                    .foregroundStyle(Color.white.opacity(0.45))
                Text("Free")
                    .font(.caption.weight(.medium))
                    .foregroundStyle(Color.white.opacity(0.7))
            }
            Spacer()
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Color(nsColor: .black))
    }

    private var slotHeader: some View {
        HStack(spacing: 8) {
            if let character = TrekCharacter.forSlot(slot.slotId),
               let nsImage = TrekIconAssets.image(character, size: .small) {
                Image(nsImage: nsImage)
                    .resizable()
                    .interpolation(.high)
                    .aspectRatio(contentMode: .fit)
                    .frame(width: 22, height: 28)
                    .clipShape(RoundedRectangle(cornerRadius: 3, style: .continuous))
            }

            VStack(alignment: .leading, spacing: 1) {
                Text(WorkerNames.name(forSlot: slot.slotId))
                    .font(.caption.weight(.semibold))
                    .foregroundStyle(.secondary)
                    .lineLimit(1)

                slotSubtitle
            }

            Spacer(minLength: 0)

            // Prefer engine-supplied LiveWorkerState — its activity is
            // driven by hook events rather than a screen-scrape that
            // always rendered "Claude Unknown". Fall back to the legacy
            // claudeState pill until the worker's first hook fires.
            if let live = liveState {
                statusPill(
                    live.activity.label,
                    color: liveActivityColor(live.activity)
                )
            } else if let state = slot.session?.claudeState {
                statusPill(state.label, color: claudeStateColor(state))
            }
        }
        .padding(.horizontal, 8)
        .padding(.vertical, 6)
        .help(slotTooltip)
    }

    private func liveActivityColor(_ activity: WorkerActivity) -> Color {
        switch activity {
        case .working: .blue
        case .waitingForInput: .orange
        case .idle: .green
        case .spawning: .secondary
        case .errored: .red
        case .terminated: .secondary
        }
    }

    private var slotTooltip: String {
        let base = "Worker \(slot.slotId)"
        if let runId = slot.runId {
            return "\(base) · run \(runId)"
        }
        return "\(base) · idle"
    }

    /// Second line in the titlebar. The engine ships a lowercase
    /// gerund phrase (e.g. `"fixing bossctl and agent stops"`) which
    /// we render as a natural-language sentence under the worker's
    /// display name (`"Riker is fixing bossctl and agent stops"`).
    /// Falls back to the run id when no summary is available so we
    /// never lose traceability. Either way we attach the run id as a
    /// hover tooltip — the sentence is purely visual and the full id
    /// is what every log/api/taxonomy elsewhere uses.
    @ViewBuilder
    private var slotSubtitle: some View {
        if let summary = slot.summary, !summary.isEmpty {
            Text("\(WorkerNames.name(forSlot: slot.slotId)) is \(summary)")
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
    let liveState: WorkerLiveState?

    var body: some View {
        GhosttyTerminalView(
            runtime: runtime,
            session: session,
            launchSpec: session.launchSpec
        )
        .background(Color(nsColor: .black))
        // Once the engine pushes a LiveWorkerState for this worker the
        // titlebar pill renders hook-driven activity and the per-pane
        // 0.5s viewport screen-scrape becomes redundant. Gate the
        // monitor so it only runs as the pre-hook fallback. `initial:
        // true` covers the spawn case where liveState is already
        // present by the time this pane mounts (e.g. a re-render after
        // a run resumed).
        .onChange(of: liveState != nil, initial: true) { _, hasLiveState in
            let enabled = !hasLiveState
            if session.claudeMonitorEnabled != enabled {
                session.claudeMonitorEnabled = enabled
            }
        }
    }
}
