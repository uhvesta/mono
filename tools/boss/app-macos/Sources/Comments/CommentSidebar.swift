import SwiftUI

/// Fixed-width (280 pt) right-side panel listing the in-memory comments for
/// the currently open markdown doc. Appears only when at least one comment
/// exists. Clicking a row jumps to its anchored text (flashes the highlighted
/// span orange for ~900 ms). Dismiss button is at the top-right of each card.
struct CommentSidebar: View {
    @ObservedObject var layer: CommentLayer

    var body: some View {
        VStack(spacing: 0) {
            header
            Divider()
            if layer.comments.isEmpty {
                Spacer()
                Text("No comments yet.")
                    .font(.callout)
                    .foregroundStyle(.secondary)
                Spacer()
            } else {
                ScrollView {
                    LazyVStack(spacing: 0) {
                        ForEach(layer.comments) { comment in
                            CommentRow(comment: comment, layer: layer)
                            Divider()
                        }
                    }
                }
            }
            addCommentRow
        }
        .background(Color(nsColor: .windowBackgroundColor))
    }

    private var header: some View {
        VStack(alignment: .leading, spacing: 2) {
            HStack {
                Text("Comments")
                    .font(.callout.weight(.semibold))
                Spacer()
                Text("\(layer.comments.count)")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .monospacedDigit()
            }
            Text("Comments not yet persisted — Phase 1 preview")
                .font(.caption2)
                .foregroundStyle(.tertiary)
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 10)
    }

    private var addCommentRow: some View {
        Button {
            layer.requestNewComment()
        } label: {
            Label("Add Comment", systemImage: "plus.bubble")
                .font(.callout)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
        .buttonStyle(.borderless)
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
    }
}

private struct CommentRow: View {
    let comment: Comment
    @ObservedObject var layer: CommentLayer

    var body: some View {
        // Entire row is tappable to jump to the comment's anchored span.
        Button {
            layer.jumpTo(comment)
        } label: {
            rowContent
        }
        .buttonStyle(.plain)
    }

    private var rowContent: some View {
        ZStack(alignment: .topTrailing) {
            VStack(alignment: .leading, spacing: 8) {
                // Leave room for the dismiss button in the top-right.
                Color.clear.frame(height: 0)
                    .padding(.trailing, 28)

                if !comment.quotedText.isEmpty {
                    Text(comment.quotedText)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .lineLimit(3)
                        .padding(.horizontal, 8)
                        .padding(.vertical, 4)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .background(
                            RoundedRectangle(cornerRadius: 4)
                                .fill(Color.accentColor.opacity(0.08))
                        )
                        .overlay(
                            RoundedRectangle(cornerRadius: 4)
                                .stroke(Color.accentColor.opacity(0.25), lineWidth: 0.5)
                        )
                }

                Text(comment.body)
                    .font(.callout)
                    .fixedSize(horizontal: false, vertical: true)

                Text(comment.createdAt, style: .relative)
                    .font(.caption2)
                    .foregroundStyle(.tertiary)
            }
            .padding(.horizontal, 12)
            .padding(.vertical, 10)

            // Dismiss button at top-right (macOS convention for dismissable cards).
            Button {
                layer.dismiss(comment)
            } label: {
                Label("Dismiss", systemImage: "xmark.circle")
                    .labelStyle(.iconOnly)
                    .font(.caption)
            }
            .buttonStyle(.borderless)
            .foregroundStyle(.secondary)
            .help("Dismiss comment")
            .padding(.top, 8)
            .padding(.trailing, 8)
        }
    }
}
