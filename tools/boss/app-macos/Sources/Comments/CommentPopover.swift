import SwiftUI

/// Popover for creating a new comment. Appears anchored near the selection.
/// Does not echo the selected text back — the yellow highlight in the viewer
/// already shows what is being commented on.
///
/// Behaviour:
///   - Plain Return submits (via CommentTextEditor's key handler).
///   - Shift+Return inserts a newline so multi-line comments are still possible.
///   - Cancel clears state without adding a comment.
struct CommentPopover: View {
    @ObservedObject var layer: CommentLayer

    @State private var commentBody: String = ""

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("New Comment")
                .font(.headline)

            CommentTextEditor(text: $commentBody, onSubmit: submit)
                .frame(minHeight: 80, maxHeight: 160)
                .overlay(
                    RoundedRectangle(cornerRadius: 6)
                        .stroke(Color(nsColor: .separatorColor), lineWidth: 0.5)
                )

            HStack {
                Spacer()
                Button("Cancel") {
                    cancel()
                }
                .keyboardShortcut(.cancelAction)

                Button("Comment") {
                    submit()
                }
                .keyboardShortcut(.defaultAction)
                .disabled(commentBody.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
            }
        }
        .padding(16)
        .frame(width: 320)
        .onAppear {
            if let first = layer.pendingFirstChar {
                commentBody = String(first)
            }
        }
    }

    private func submit() {
        layer.addComment(quoted: layer.pendingQuotedText, body: commentBody)
        commentBody = ""
    }

    private func cancel() {
        layer.isShowingPopover = false
        layer.pendingFirstChar = nil
        commentBody = ""
    }
}
