import SwiftUI

// Notifications window (design: tools/boss/docs/designs/attentions.md — "App
// UI"). Renders the selected product's attention groups as grouped cards:
// a kind chip, association jump links, a source summary, member count, an
// `extracted` badge, and the kind-appropriate inline member controls
// (yes_no / multiple_choice / prompt / followup accept-reject). The card
// footer actions the whole group via `ActionAttentionGroup`; an actioned
// group leaves the open list and lingers in a "Recently resolved" section
// with a link to the produced revision / tasks.

/// Root content of the singleton `attentions` window.
struct AttentionsView: View {
    @EnvironmentObject private var model: ChatViewModel

    var body: some View {
        VStack(spacing: 0) {
            header
            Divider()
            content
        }
        .frame(minWidth: 380, minHeight: 320)
        .background(Color(nsColor: .windowBackgroundColor))
    }

    private var header: some View {
        HStack(spacing: 8) {
            Image(systemName: "bell.badge")
                .foregroundStyle(.secondary)
            Text("Notifications")
                .font(.headline)
            if let product = model.selectedProduct {
                Text(product.name)
                    .font(.subheadline)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
            Spacer(minLength: 0)
            if model.openAttentionGroupCount > 0 {
                Text("\(model.openAttentionGroupCount) open")
                    .font(.caption.weight(.semibold))
                    .foregroundStyle(.secondary)
            }
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 12)
    }

    @ViewBuilder
    private var content: some View {
        if model.selectedProduct == nil {
            emptyState(
                icon: "tray",
                title: "No product selected",
                message: "Pick a product in the main window to see its notifications."
            )
        } else if openGroups.isEmpty && resolvedGroups.isEmpty {
            emptyState(
                icon: "checkmark.circle",
                title: "All caught up",
                message: "No questions or followups are waiting on you."
            )
        } else {
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 12) {
                    ForEach(openGroups) { group in
                        AttentionGroupCard(group: group)
                    }
                    if !resolvedGroups.isEmpty {
                        Text("Recently resolved")
                            .font(.caption.weight(.semibold))
                            .foregroundStyle(.secondary)
                            .padding(.top, 4)
                        ForEach(resolvedGroups) { group in
                            AttentionResolvedCard(group: group)
                        }
                    }
                }
                .padding(16)
            }
        }
    }

    private var openGroups: [AttentionGroup] {
        model.selectedProductOpenAttentionGroups
    }

    /// Groups that flipped to `actioned` this session — shown transiently so
    /// the produced-artifact link is reachable until the next full reload.
    private var resolvedGroups: [AttentionGroup] {
        model.selectedProductAttentionGroups.filter { $0.isActioned && !$0.producedArtifacts.isEmpty }
    }

    private func emptyState(icon: String, title: String, message: String) -> some View {
        VStack(spacing: 8) {
            Image(systemName: icon)
                .font(.system(size: 32))
                .foregroundStyle(.tertiary)
            Text(title)
                .font(.headline)
            Text(message)
                .font(.subheadline)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .padding(32)
    }
}

/// One open / partially-answered group, expandable to its member controls.
private struct AttentionGroupCard: View {
    @EnvironmentObject private var model: ChatViewModel
    let group: AttentionGroup
    @State private var isExpanded = true
    /// Local drafts for `prompt`-type questions, keyed by attention id. Seeded
    /// from the persisted answer; flushed to the engine on submit.
    @State private var promptDrafts: [String: String] = [:]
    @State private var showingAssociationPopover = false

    private var members: [Attention] { model.attentionMembers(forGroup: group.id) }
    private var answeredCount: Int { members.filter(\.isAnswered).count }
    private var hasExtracted: Bool { members.contains { $0.confidenceSource == "extracted" } }

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            cardHeader
            if isExpanded {
                Divider()
                VStack(alignment: .leading, spacing: 12) {
                    ForEach(members) { member in
                        AttentionMemberRow(
                            group: group,
                            member: member,
                            promptDraft: promptBinding(for: member)
                        )
                    }
                }
                footer
            }
        }
        .padding(14)
        .background(
            RoundedRectangle(cornerRadius: 10)
                .fill(Color(nsColor: .controlBackgroundColor))
        )
        .overlay(
            RoundedRectangle(cornerRadius: 10)
                .strokeBorder(Color(nsColor: .separatorColor), lineWidth: 1)
        )
    }

    private var cardHeader: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 8) {
                AttentionChip(text: group.kindLabel, system: group.kind == "question" ? "questionmark.circle" : "arrow.up.forward.app")
                if hasExtracted {
                    AttentionChip(text: "Extracted", system: "sparkle.magnifyingglass", tint: .orange)
                }
                Spacer(minLength: 0)
                Text("\(members.count) \(members.count == 1 ? "item" : "items")")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                Button {
                    withAnimation(.easeInOut(duration: 0.15)) { isExpanded.toggle() }
                } label: {
                    Image(systemName: isExpanded ? "chevron.up" : "chevron.down")
                        .font(.caption.weight(.semibold))
                }
                .buttonStyle(.plain)
                .help(isExpanded ? "Collapse" : "Expand")
            }
            associationLinks
            if let summary = sourceSummary {
                Text(summary)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(2)
            }
        }
    }

    @ViewBuilder
    private var associationLinks: some View {
        VStack(alignment: .leading, spacing: 3) {
            HStack(spacing: 10) {
                Button {
                    if group.associationTaskID != nil {
                        showingAssociationPopover = true
                    } else {
                        model.revealAttentionAssociation(group)
                    }
                } label: {
                    Label(
                        model.attentionAssociationLabel(group),
                        systemImage: group.associationTaskID != nil ? "square.stack.3d.up" : "folder"
                    )
                    .font(.caption)
                }
                .buttonStyle(.link)
                .popover(isPresented: $showingAssociationPopover) {
                    if let task = model.attentionAssociationTask(group) {
                        WorkCardPopoverView(model: model, task: task)
                    }
                }

                if group.kind == "question", group.associationProjectID != nil {
                    Button {
                        model.openAttentionDesignDoc(group)
                    } label: {
                        Label("Design doc", systemImage: "doc.richtext")
                            .font(.caption)
                    }
                    .buttonStyle(.link)
                }
            }
            if let taskName = model.attentionAssociationTask(group)?.name {
                Text(taskName)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(2)
            }
        }
    }

    private var sourceSummary: String? {
        switch group.sourceKind {
        case "design_doc":
            return group.sourceDocPath.map { "From design doc \($0)" }
        case "task_transcript":
            return "From a task transcript"
        case "manual":
            return "Raised manually"
        default:
            return nil
        }
    }

    @ViewBuilder
    private var footer: some View {
        if group.kind != "followup" {
            HStack(spacing: 10) {
                Spacer(minLength: 0)
                Button {
                    submit()
                } label: {
                    Text("Submit answers")
                }
                .buttonStyle(.borderedProminent)
                .disabled(!canSubmit)
                .help(canSubmit
                      ? "Action this group — unanswered items are skipped"
                      : "Answer or accept at least one item first")
            }
        }
    }

    /// Enable the footer once at least one member is answered, or a `prompt`
    /// member has unsaved text (which `submit()` flushes before actioning).
    private var canSubmit: Bool {
        answeredCount > 0 || hasPendingPromptDraft
    }

    private var hasPendingPromptDraft: Bool {
        members.contains { member in
            member.questionType == "prompt"
                && !(promptDrafts[member.id] ?? "")
                    .trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        }
    }

    private func promptBinding(for member: Attention) -> Binding<String> {
        Binding(
            get: { promptDrafts[member.id] ?? member.answer ?? "" },
            set: { promptDrafts[member.id] = $0 }
        )
    }

    /// Flush any edited prompt drafts, then action the group. Sends share one
    /// ordered socket, so the answers are processed before the action.
    private func submit() {
        for member in members where member.questionType == "prompt" {
            let draft = (promptDrafts[member.id] ?? "").trimmingCharacters(in: .whitespacesAndNewlines)
            if !draft.isEmpty, draft != (member.answer ?? "") {
                model.answerAttention(member.id, answer: draft)
            }
        }
        model.actionAttentionGroup(group.id)
    }
}

/// One member row: the kind-appropriate inline control plus a skip affordance.
private struct AttentionMemberRow: View {
    @EnvironmentObject private var model: ChatViewModel
    let group: AttentionGroup
    let member: Attention
    @Binding var promptDraft: String

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(alignment: .center, spacing: 6) {
                if group.kind != "followup" {
                    stateGlyph
                }
                Text(title)
                    .font(.subheadline.weight(.medium))
                    .fixedSize(horizontal: false, vertical: true)
            }
            control
            // yes_no / multiple_choice need a discrete skip; prompt has its
            // own Skip button and followup has a Reject segment.
            if group.kind != "followup" && member.questionType != "prompt" {
                skipRow
            }
        }
        .opacity(member.isResolved && !member.isAnswered ? 0.55 : 1)
    }

    private var title: String {
        if group.kind == "followup" {
            return member.proposedName ?? "Proposed work"
        }
        return member.promptText ?? "Question"
    }

    @ViewBuilder
    private var stateGlyph: some View {
        switch member.answerState {
        case "answered":
            Image(systemName: "checkmark.circle.fill").foregroundStyle(.green)
        case "skipped":
            Image(systemName: "minus.circle").foregroundStyle(.secondary)
        case "dismissed":
            Image(systemName: "xmark.circle").foregroundStyle(.secondary)
        default:
            Image(systemName: "circle").foregroundStyle(.tertiary)
        }
    }

    @ViewBuilder
    private var control: some View {
        if group.kind == "followup" {
            followupControl
        } else {
            switch member.questionType {
            case "yes_no":
                yesNoControl
            case "multiple_choice":
                multipleChoiceControl
            default:
                promptControl
            }
        }
    }

    // MARK: question controls

    private var yesNoControl: some View {
        Picker("", selection: yesNoBinding) {
            Text("—").tag("")
            Text("Yes").tag("yes")
            Text("No").tag("no")
        }
        .labelsHidden()
        .pickerStyle(.segmented)
        .frame(maxWidth: 220, alignment: .leading)
    }

    private var yesNoBinding: Binding<String> {
        Binding(
            get: { member.answer ?? "" },
            set: { value in
                guard value == "yes" || value == "no" else { return }
                model.answerAttention(member.id, answer: value)
            }
        )
    }

    private var multipleChoiceControl: some View {
        VStack(alignment: .leading, spacing: 4) {
            ForEach(member.choices, id: \.self) { choice in
                Button {
                    model.answerAttention(member.id, answer: choice)
                } label: {
                    HStack(spacing: 6) {
                        Image(systemName: member.answer == choice ? "largecircle.fill.circle" : "circle")
                            .foregroundStyle(member.answer == choice ? Color.accentColor : .secondary)
                        Text(choice)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                    .font(.callout)
                }
                .buttonStyle(.plain)
            }
            if member.choices.isEmpty {
                Text("No choices provided.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }
    }

    private var promptControl: some View {
        VStack(alignment: .leading, spacing: 4) {
            TextField("Type your answer…", text: $promptDraft, axis: .vertical)
                .textFieldStyle(.roundedBorder)
                .lineLimit(2...6)
            HStack(spacing: 8) {
                Button("Save") {
                    let trimmed = promptDraft.trimmingCharacters(in: .whitespacesAndNewlines)
                    guard !trimmed.isEmpty else { return }
                    model.answerAttention(member.id, answer: trimmed)
                }
                .controlSize(.small)
                .disabled(promptDraft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
                Button("Skip") {
                    model.skipAttention(member.id)
                }
                .controlSize(.small)
                .buttonStyle(.plain)
                .foregroundStyle(.secondary)
            }
        }
    }

    // MARK: followup control

    private var followupControl: some View {
        VStack(alignment: .leading, spacing: 6) {
            if let description = member.proposedDescription, !description.isEmpty {
                Text(description)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            HStack(spacing: 8) {
                if let effort = member.proposedEffort {
                    AttentionChip(text: effort.capitalized, system: "gauge.medium", tint: .secondary)
                }
                if let kind = member.proposedWorkKind {
                    AttentionChip(text: kind.capitalized, system: "square.dashed", tint: .secondary)
                }
            }
            Picker("", selection: followupBinding) {
                Text("Accept").tag("accept")
                Text("Reject").tag("reject")
            }
            .labelsHidden()
            .pickerStyle(.segmented)
            .frame(maxWidth: 160, alignment: .leading)
        }
    }

    private var followupBinding: Binding<String> {
        Binding(
            get: {
                switch member.answerState {
                case "answered": return "accept"
                case "skipped", "dismissed": return "reject"
                default: return ""
                }
            },
            set: { value in
                switch value {
                case "accept":
                    model.acceptFollowup(member.id)
                    model.actionAttentionGroup(group.id)
                case "reject":
                    model.skipAttention(member.id)
                    model.dismissAttentionGroup(group.id)
                default: break
                }
            }
        )
    }

    private var skipRow: some View {
        Button("Skip") {
            model.skipAttention(member.id)
        }
        .controlSize(.small)
        .buttonStyle(.plain)
        .font(.caption)
        .foregroundStyle(.secondary)
    }
}

/// A group that has been actioned this session — shows the produced artifact
/// with a jump link, no controls.
private struct AttentionResolvedCard: View {
    @EnvironmentObject private var model: ChatViewModel
    let group: AttentionGroup

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 8) {
                Image(systemName: "checkmark.seal.fill")
                    .foregroundStyle(.green)
                Text(producedTitle)
                    .font(.subheadline.weight(.medium))
                Spacer(minLength: 0)
            }
            ForEach(group.producedArtifacts, id: \.taskID) { ref in
                Button {
                    model.revealProducedArtifact(ref)
                } label: {
                    Label(producedLabel(ref), systemImage: "arrow.right.circle")
                        .font(.caption)
                }
                .buttonStyle(.link)
            }
        }
        .padding(12)
        .background(
            RoundedRectangle(cornerRadius: 10)
                .fill(Color(nsColor: .controlBackgroundColor).opacity(0.6))
        )
    }

    private var producedTitle: String {
        switch group.producedArtifactKind {
        case "revision": return "Created a doc revision"
        case "design_task": return "Created a design task"
        case "tasks": return "Created tasks"
        default: return "Actioned"
        }
    }

    private func producedLabel(_ ref: ProducedArtifactRef) -> String {
        let prefix = ref.kind == "chore" ? "Chore" : "Task"
        if let shortID = ref.shortID {
            return "\(prefix) \(ref.kind == "chore" ? "C" : "T")\(shortID)"
        }
        return prefix
    }
}

/// Small capsule chip used for kind / extracted / effort labels.
private struct AttentionChip: View {
    let text: String
    var system: String?
    var tint: Color = .accentColor

    var body: some View {
        HStack(spacing: 4) {
            if let system {
                Image(systemName: system)
            }
            Text(text)
        }
        .font(.caption2.weight(.semibold))
        .padding(.horizontal, 7)
        .padding(.vertical, 3)
        .background(Capsule().fill(tint.opacity(0.15)))
        .foregroundStyle(tint == .secondary ? Color.secondary : tint)
    }
}
