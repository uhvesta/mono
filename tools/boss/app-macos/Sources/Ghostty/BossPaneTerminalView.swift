import AppKit
import SwiftUI

/// SwiftUI host for the Boss session's libghostty pane. Renders just
/// the terminal surface — pane chrome (title, collapse) lives in the
/// parent's single `bossAgentHeader` row.
struct BossPaneTerminalView: View {
    @ObservedObject var boss: BossPaneModel

    var body: some View {
        BossTerminalSurface(runtime: boss.runtime, session: boss.session)
            .background(Color(nsColor: .black))
    }
}

private struct BossTerminalSurface: View {
    let runtime: GhosttyRuntime
    @ObservedObject var session: TerminalPaneSession

    var body: some View {
        GhosttyTerminalView(
            runtime: runtime,
            session: session,
            launchSpec: session.launchSpec,
            // Boss panes never display `claudeState`, so the screen
            // scrape stays off.
            claudeMonitorEnabled: false
        )
    }
}
