import AppKit
import SwiftUI

/// SwiftUI host for the Boss session's libghostty pane. Renders just
/// the terminal surface — pane chrome (title, collapse, status pill)
/// lives in the parent's single `bossAgentHeader` row.
struct BossPaneTerminalView: View {
    @ObservedObject var boss: BossPaneModel

    var body: some View {
        BossTerminalSurface(runtime: boss.runtime, session: boss.session)
            .background(Color(nsColor: .black))
    }
}

/// Status pill rendered in the Boss pane header. Only appears when
/// Claude is detected and meaningfully ready/working — `unavailable`
/// and `notDetected` states are intentionally silent so the header
/// doesn't carry a misleading "Claude Unknown" tag.
struct BossClaudeStatusPill: View {
    @ObservedObject var session: TerminalPaneSession

    var body: some View {
        switch session.claudeState {
        case .ready, .working:
            pill(session.claudeState.label, color: color(for: session.claudeState))
        case .unavailable, .notDetected:
            EmptyView()
        }
    }

    private func pill(_ text: String, color: Color) -> some View {
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

    private func color(for state: ClaudeMonitorState) -> Color {
        switch state {
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
