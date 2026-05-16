import AppKit
import SwiftUI

/// Owns the in-memory comment array for a single markdown viewer instance
/// and coordinates the selection → authoring → sidebar → highlight flow.
///
/// Phase 1: all state is in-memory; no engine RPCs; closing the viewer
/// loses all comments. This is intentional and surfaced to the user in
/// the sidebar header.
@MainActor
final class CommentLayer: ObservableObject {
    @Published var comments: [Comment] = []
    @Published var isShowingPopover: Bool = false
    @Published var pendingQuotedText: String = ""

    func requestNewComment() {
        pendingQuotedText = captureCurrentSelection() ?? ""
        isShowingPopover = true
    }

    func addComment(quoted: String, body: String) {
        guard !body.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { return }
        let comment = Comment(
            id: UUID(),
            quotedText: quoted,
            body: body.trimmingCharacters(in: .whitespacesAndNewlines),
            createdAt: Date()
        )
        comments.append(comment)
        isShowingPopover = false
        pendingQuotedText = ""
    }

    func dismiss(_ comment: Comment) {
        comments.removeAll { $0.id == comment.id }
    }

    /// Attempts to read the currently selected text by sending the standard
    /// copy action to the first responder and reading the pasteboard result.
    /// Called only on an explicit "Add Comment" click, so clobbering the
    /// pasteboard is an acceptable Phase 1 trade-off.
    private func captureCurrentSelection() -> String? {
        let before = NSPasteboard.general.changeCount
        NSApp.sendAction(#selector(NSText.copy(_:)), to: nil, from: nil)
        guard NSPasteboard.general.changeCount != before else { return nil }
        return NSPasteboard.general.string(forType: .string)
    }
}

// MARK: - View modifier

/// Wraps a markdown viewer with the full comment affordance:
/// sidebar (when comments exist), "Add Comment" button, and authoring sheet.
///
/// Usage:
/// ```swift
/// MarkdownViewerView(...)
///     .withComments()
/// ```
struct WithCommentsModifier: ViewModifier {
    @StateObject private var layer = CommentLayer()

    func body(content: Content) -> some View {
        HStack(spacing: 0) {
            content
            if !layer.comments.isEmpty {
                Divider()
                CommentSidebar(layer: layer)
                    .frame(width: 280)
            }
        }
        .overlay(alignment: .topTrailing) {
            if layer.comments.isEmpty {
                addCommentButton
                    .padding(.trailing, 16)
                    .padding(.top, 20)
            }
        }
        .sheet(isPresented: $layer.isShowingPopover) {
            CommentPopover(layer: layer)
        }
    }

    private var addCommentButton: some View {
        Button {
            layer.requestNewComment()
        } label: {
            Label("Add Comment", systemImage: "bubble.left.and.text.bubble.right")
                .font(.callout)
        }
        .buttonStyle(.bordered)
        .controlSize(.small)
        .help("Select text then click to add a comment")
    }
}

extension View {
    func withComments() -> some View {
        modifier(WithCommentsModifier())
    }
}
