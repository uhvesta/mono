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
    /// Human-readable task name, e.g. "Fix the fencer scraper".
    var taskName: String?
    /// Per-product short id, e.g. 808. Displayed as "T808".
    var taskShortID: Int?

    var id: String { workItemID }

    /// Formatted window title: "Review: T<n> - <name>" when both are
    /// available; falls back gracefully when either is missing.
    var windowTitle: String {
        let prefix = "Review"
        switch (taskShortID, taskName) {
        case let (shortID?, name?) where !name.isEmpty:
            return "\(prefix): T\(shortID) - \(name)"
        case let (shortID?, _):
            return "\(prefix): T\(shortID)"
        case let (_, name?) where !name.isEmpty:
            return "\(prefix): \(name)"
        default:
            return "\(prefix): \(workItemID)"
        }
    }
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
