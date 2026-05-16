import SwiftUI

/// Authoring sheet for a new comment. Pre-populates the quoted-text field
/// with whatever text was selected when "Add Comment" was clicked. The user
/// can edit the snippet and must supply a non-empty comment body before
/// the "Comment" button enables.
struct CommentPopover: View {
    @ObservedObject var layer: CommentLayer

    @State private var quotedText: String = ""
    @State private var commentBody: String = ""

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            Text("Add Comment")
                .font(.headline)

            if !quotedText.isEmpty {
                VStack(alignment: .leading, spacing: 4) {
                    Text("Selected text")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                    Text(quotedText)
                        .font(.callout)
                        .foregroundStyle(.secondary)
                        .lineLimit(4)
                        .padding(8)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .background(
                            RoundedRectangle(cornerRadius: 6)
                                .fill(Color(nsColor: .quaternaryLabelColor).opacity(0.18))
                        )
                }
            }

            VStack(alignment: .leading, spacing: 4) {
                Text("Comment")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                TextEditor(text: $commentBody)
                    .font(.callout)
                    .frame(minHeight: 80, maxHeight: 160)
                    .overlay(
                        RoundedRectangle(cornerRadius: 6)
                            .stroke(Color(nsColor: .separatorColor), lineWidth: 0.5)
                    )
            }

            HStack {
                Spacer()
                Button("Cancel") {
                    layer.isShowingPopover = false
                    commentBody = ""
                }
                .keyboardShortcut(.cancelAction)

                Button("Comment") {
                    layer.addComment(quoted: quotedText, body: commentBody)
                    commentBody = ""
                }
                .keyboardShortcut(.defaultAction)
                .disabled(commentBody.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
            }
        }
        .padding(20)
        .frame(width: 360)
        .onAppear {
            quotedText = layer.pendingQuotedText
        }
    }
}
