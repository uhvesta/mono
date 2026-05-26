import AppKit
import SwiftUI

struct WorkersDetailView: View {
    @ObservedObject var workspace: WorkersWorkspaceModel
    @ObservedObject var liveStates: LiveWorkerStateStore
    /// View model that owns the per-slot live-status enabled flags
    /// and exposes the RPC to toggle them. Plumbed in from the
    /// `ContentView` parent so this view can stay a thin shell over
    /// `ChatViewModel`'s state.
    @ObservedObject var liveStatusModel: ChatViewModel

    var body: some View {
        WorkerGrid(
            runtime: workspace.runtime,
            slots: workspace.slots,
            liveStates: liveStates,
            liveStatusModel: liveStatusModel
        )
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Color(nsColor: .separatorColor))
    }
}

private struct WorkerGrid: View {
    let runtime: GhosttyRuntime
    let slots: [WorkerSlot]
    @ObservedObject var liveStates: LiveWorkerStateStore
    @ObservedObject var liveStatusModel: ChatViewModel

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
                            liveState: liveStates.bySlot[slot.slotId],
                            liveStatusEnabled: liveStatusModel.isLiveStatusEnabled(slotId: slot.slotId),
                            onToggleLiveStatus: { enabled in
                                liveStatusModel.setLiveStatusEnabled(
                                    slotId: slot.slotId,
                                    enabled: enabled
                                )
                            }
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
    /// Whether the live-status summarizer is currently enabled for
    /// this slot. Drives the small toggle in the slot header (Q9
    /// per-worker off-switch).
    let liveStatusEnabled: Bool
    /// Closure the toggle calls when the human flips the switch. The
    /// parent threads this through to `ChatViewModel`, which sends
    /// the RPC and updates the local mirror.
    let onToggleLiveStatus: (Bool) -> Void

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
                slotTaskLine

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

            liveStatusToggle
        }
        .padding(.horizontal, 8)
        .padding(.vertical, 6)
        .help(slotTooltip)
    }

    /// Tiny eye-icon toggle in the slot header that disables the
    /// live-status summarizer for this slot. Off = engine stops
    /// summarising and the UI falls back to pane_summary. Persisted
    /// across engine restarts; the engine echoes the new state back
    /// so reads via subscribe stay in sync. Tooltip explains the
    /// trade-off so a curious user understands what the icon does.
    @ViewBuilder
    private var liveStatusToggle: some View {
        let enabled = liveStatusEnabled
        Button {
            onToggleLiveStatus(!enabled)
        } label: {
            Image(systemName: enabled ? "eye" : "eye.slash")
                .font(.caption2)
                .foregroundStyle(enabled ? Color.secondary : Color(nsColor: .tertiaryLabelColor))
        }
        .buttonStyle(.plain)
        .help(
            enabled
                ? "Live status on — engine summarises this worker's transcript."
                : "Live status off — falls back to the static pane summary."
        )
        .accessibilityLabel(enabled ? "Disable live status" : "Enable live status")
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

    /// First line in the titlebar — the overall task this worker is
    /// on, in gerund form, with the worker name as subject. Stable
    /// for the duration of a run (sourced from
    /// `engine/src/pane_summary.rs`, which caches a Claude-generated
    /// gerund phrase per work item). For an idle slot or while the
    /// engine hasn't shipped a summary yet, falls back to the bare
    /// worker name (active run) or just the name (idle) — never
    /// empty, never grammatically broken.
    @ViewBuilder
    private var slotTaskLine: some View {
        let name = WorkerNames.name(forSlot: slot.slotId)
        let text: String = {
            if let summary = slot.summary, !summary.isEmpty {
                return "\(name) is \(summary)"
            }
            if slot.runId != nil {
                return "\(name) is working"
            }
            return name
        }()
        Text(text)
            .font(.caption.weight(.semibold))
            .foregroundStyle(.secondary)
            .lineLimit(1)
            .help(slot.runId ?? "")
    }

    /// Second line in the titlebar — the engine's real-time
    /// live-status sentence (refreshed by the summarizer in
    /// `engine/src/live_status.rs`). When no live status is
    /// available we fall through to the run id and then to "idle"
    /// so the line still anchors the pane visually. The static
    /// pane-summary gerund is rendered on the first line via
    /// `slotTaskLine` and intentionally not duplicated here.
    @ViewBuilder
    private var slotSubtitle: some View {
        if let live = liveState?.liveStatus,
           !live.isEmpty
        {
            HStack(alignment: .firstTextBaseline, spacing: 4) {
                WorkerWaitingIndicator(
                    activity: liveState?.activity,
                    lastEventAt: liveState?.lastEventAt
                )
                Text(live)
                    .font(.caption2)
                    .foregroundStyle(liveStatusColor)
                    .lineLimit(1)
                    .help(slot.runId ?? "")
                    .accessibilityLabel("Live status: \(live)")
            }
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

    /// Match the Doing-card colour mapping: red for errored runs,
    /// tertiary for idle, `.secondary` otherwise. `waitingForInput`
    /// is no longer tinted accent-blue — it surfaces the explicit
    /// `WorkerWaitingIndicator` icon + tooltip in `slotSubtitle`
    /// instead, so the meaning is not carried by hue alone.
    private var liveStatusColor: Color {
        switch liveState?.activity {
        case .errored:
            return .red
        case .idle:
            return Color(nsColor: .tertiaryLabelColor)
        default:
            return .secondary
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
