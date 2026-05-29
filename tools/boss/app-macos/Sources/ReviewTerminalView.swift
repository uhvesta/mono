import AppKit
import SwiftUI

/// Value type passed to the `"review-terminal"` WindowGroup to identify
/// a single review-terminal window. Codable so SwiftUI can restore the
/// window across app relaunches; Hashable + Identifiable so the group
/// de-dupes by workItemID.
struct ReviewTerminalContent: Codable, Hashable, Identifiable {
    let workItemID: String
    let workspacePath: String
    let leaseID: String

    var id: String { workItemID }
}

/// Full-window Ghostty terminal opened from a Review-column card's
/// terminal button. The working directory is the cube-leased workspace
/// that the engine set up on the PR branch (`jj new -r <branch>@origin`).
///
/// Lease lifecycle: the engine holds the lease while this view is on
/// screen. When the window closes (`onDisappear`) we send
/// `release_review_terminal` so the workspace returns to the pool.
struct ReviewTerminalView: View {
    let content: ReviewTerminalContent
    @EnvironmentObject private var chatModel: ChatViewModel

    var body: some View {
        ReviewTerminalSurface(content: content)
            .onDisappear {
                chatModel.releaseReviewTerminal(leaseID: content.leaseID)
            }
    }
}

/// NSViewRepresentable wrapper that hosts the Ghostty surface for the
/// review terminal. Creates a fresh `TerminalPaneSession` each time the
/// view is constructed (i.e. once per window).
private struct ReviewTerminalSurface: View {
    let content: ReviewTerminalContent
    @StateObject private var session: TerminalPaneSession

    init(content: ReviewTerminalContent) {
        self.content = content
        let spec = TerminalLaunchSpec(
            fontSize: 12.0,
            workingDirectory: content.workspacePath,
            initialInput: ""
        )
        _session = StateObject(wrappedValue: TerminalPaneSession(
            id: "review-terminal-\(content.workItemID)",
            role: .boss,
            launchSpec: spec
        ))
    }

    var body: some View {
        GhosttyTerminalView(
            runtime: GhosttyRuntime.shared,
            session: session,
            launchSpec: session.launchSpec,
            claudeMonitorEnabled: false
        )
        .background(Color(nsColor: .black))
    }
}
