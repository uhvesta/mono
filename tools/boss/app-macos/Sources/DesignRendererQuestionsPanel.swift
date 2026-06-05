import SwiftUI

// Collapsible questions sidebar for DesignRendererView. Lists open question
// groups whose source_doc_path matches the on-screen doc, with the same
// AttentionGroupCard controls as the Attentions window so answering here
// drives the identical AnswerAttention / ActionAttentionGroup RPCs.
//
// Placement: anchored questions render next to their section by showing the
// source_anchor slug (§ heading-slug) above each question prompt in
// AttentionMemberRow. Full inline placement alongside markdown headings is a
// follow-up pending the anchoring substrate from the comments-in-markdown
// design (P529).

/// Fixed-width right-side panel for DesignRendererView showing open question
/// groups that concern the displayed design doc.
struct DesignQuestionsPanel: View {
    @EnvironmentObject private var model: ChatViewModel
    let groups: [AttentionGroup]
    @State private var isExpanded = true

    var body: some View {
        VStack(spacing: 0) {
            panelHeader
            Divider()
            if isExpanded {
                panelContent
            }
        }
        .background(Color(nsColor: .windowBackgroundColor))
    }

    private var panelHeader: some View {
        HStack(spacing: 8) {
            Image(systemName: "questionmark.circle.fill")
                .foregroundStyle(Color.accentColor)
            Text("Questions")
                .font(.callout.weight(.semibold))
            Text("\(groups.count)")
                .font(.caption.weight(.semibold))
                .foregroundStyle(.secondary)
                .monospacedDigit()
            Spacer(minLength: 0)
            Button {
                withAnimation(.easeInOut(duration: 0.15)) {
                    isExpanded.toggle()
                }
            } label: {
                Image(systemName: isExpanded ? "chevron.right" : "chevron.left")
                    .font(.caption.weight(.semibold))
            }
            .buttonStyle(.plain)
            .help(isExpanded ? "Collapse questions" : "Expand questions")
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 10)
    }

    @ViewBuilder
    private var panelContent: some View {
        ScrollView {
            LazyVStack(alignment: .leading, spacing: 12) {
                ForEach(groups) { group in
                    AttentionGroupCard(group: group)
                }
            }
            .padding(12)
        }
    }
}
