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
            BossTerminalSurface(runtime: boss.runtime, session: boss.session)
                .background(Color(nsColor: .black))
        }
    }

    private var statusRow: some View {
        HStack(spacing: 8) {
            Text(boss.session.displayTitle)
                .font(.caption2.weight(.semibold))
                .foregroundStyle(.secondary)
                .textCase(.uppercase)
                .tracking(0.4)
                .lineLimit(1)
            Spacer(minLength: 0)
            statusPill(boss.session.claudeState.label, color: claudeStateColor)
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 6)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(Color(nsColor: .controlBackgroundColor))
        .overlay(alignment: .bottom) {
            Rectangle()
                .fill(Color(nsColor: .separatorColor).opacity(0.6))
                .frame(height: 0.5)
        }
    }

    private func statusPill(_ text: String, color: Color) -> some View {
        HStack(spacing: 5) {
            Circle()
                .fill(color)
                .frame(width: 6, height: 6)
            Text(text)
                .font(.caption2.weight(.semibold))
                .lineLimit(1)
                .foregroundStyle(color)
        }
        .padding(.horizontal, 8)
        .padding(.vertical, 3)
        .background(
            Capsule()
                .fill(color.opacity(0.12))
        )
        .overlay(
            Capsule()
                .stroke(color.opacity(0.22), lineWidth: 0.5)
        )
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
