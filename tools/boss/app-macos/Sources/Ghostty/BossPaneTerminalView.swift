import AppKit
import SwiftUI

/// SwiftUI host for the Boss session's libghostty pane. Mirrors the
/// header + terminal layout of the worker grid cells but for the
/// single Boss session, sized to fill the docked Boss panel in Work
/// mode.
struct BossPaneTerminalView: View {
    @ObservedObject var boss: BossPaneModel

    var body: some View {
        VStack(spacing: 0) {
            statusRow
            Divider()
            BossTerminalSurface(runtime: boss.runtime, session: boss.session)
                .background(Color(nsColor: .black))
        }
    }

    private var statusRow: some View {
        HStack(spacing: 8) {
            Text(boss.session.displayTitle)
                .font(.caption.weight(.medium))
                .foregroundStyle(.secondary)
                .lineLimit(1)
            Spacer(minLength: 0)
            statusPill(boss.session.claudeState.label, color: claudeStateColor)
        }
        .padding(.horizontal, 10)
        .padding(.vertical, 6)
    }

    private func statusPill(_ text: String, color: Color) -> some View {
        Text(text)
            .font(.caption2.weight(.medium))
            .lineLimit(1)
            .padding(.horizontal, 8)
            .padding(.vertical, 3)
            .background(color.opacity(0.14))
            .foregroundStyle(color)
            .clipShape(Capsule())
    }

    private var claudeStateColor: Color {
        switch boss.session.claudeState {
        case .working: .blue
        case .ready: .green
        case .notDetected: .secondary
        case .unavailable: .orange
        }
    }
}

private struct BossTerminalSurface: View {
    let runtime: GhosttyRuntime
    @ObservedObject var session: TerminalPaneSession

    var body: some View {
        GhosttyTerminalView(
            runtime: runtime,
            session: session,
            launchSpec: session.launchSpec
        )
    }
}
