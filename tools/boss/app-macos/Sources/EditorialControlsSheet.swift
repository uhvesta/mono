import SwiftUI

// MARK: - Top-level sheet

/// Sheet presented when the operator clicks the editorial-controls button in
/// the Work sidebar. Shows the current `EditorialRules` for the selected
/// product (editable) and a recent audit trail of enforcement actions.
struct EditorialControlsSheet: View {
    @ObservedObject var model: ChatViewModel
    let productID: String
    let onDismiss: () -> Void

    @State private var tab: EditorialTab = .rules

    private var product: WorkProduct? {
        model.products.first { $0.id == productID }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            // Header
            HStack {
                VStack(alignment: .leading, spacing: 2) {
                    Text("Editorial Rules")
                        .font(.title3.weight(.semibold))
                    if let name = product?.name {
                        Text(name)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                }
                Spacer()
                Button("Done", action: onDismiss)
                    .keyboardShortcut(.defaultAction)
            }
            .padding(.horizontal, 20)
            .padding(.top, 20)
            .padding(.bottom, 12)

            Divider()

            Picker("Tab", selection: $tab) {
                ForEach(EditorialTab.allCases) { t in
                    Text(t.label).tag(t)
                }
            }
            .pickerStyle(.segmented)
            .padding(.horizontal, 20)
            .padding(.vertical, 12)

            Divider()

            Group {
                switch tab {
                case .rules:
                    if let product {
                        EditorialRulesEditor(model: model, product: product)
                    } else {
                        Text("Product not found.")
                            .foregroundStyle(.secondary)
                            .frame(maxWidth: .infinity, maxHeight: .infinity)
                    }
                case .actions:
                    EditorialActionsPane(model: model, productID: productID)
                }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
        .frame(width: 620, height: 560)
    }
}

private enum EditorialTab: String, CaseIterable, Identifiable {
    case rules, actions
    var id: String { rawValue }
    var label: String {
        switch self {
        case .rules: return "Rules"
        case .actions: return "Recent Actions"
        }
    }
}

// MARK: - Rules editor

private struct EditorialRulesEditor: View {
    @ObservedObject var model: ChatViewModel
    let product: WorkProduct

    @State private var branchNaming: EditorialBranchNaming
    @State private var customPrefix: String
    @State private var trailerPolicy: EditorialTrailerPolicy
    @State private var templatePolicy: EditorialTemplatePolicy
    @State private var instructions: String
    @State private var redactions: [EditorialRedactionRule]
    @State private var isSaving = false

    init(model: ChatViewModel, product: WorkProduct) {
        self.model = model
        self.product = product
        let rules = product.editorialRules ?? EditorialRules()
        _branchNaming = State(initialValue: rules.branchNaming)
        _customPrefix = State(
            initialValue: {
                if case .customPrefix(let p) = rules.branchNaming { return p }
                return ""
            }()
        )
        _trailerPolicy = State(initialValue: rules.commitTrailerPolicy)
        _templatePolicy = State(initialValue: rules.templatePolicy)
        _instructions = State(initialValue: rules.instructions ?? "")
        _redactions = State(initialValue: rules.redactions)
    }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 20) {
                // Branch naming
                EditorialSection("Branch Naming") {
                    Picker("Strategy", selection: $branchNaming) {
                        Text("Boss prefix (boss/exec_<id>)").tag(EditorialBranchNaming.bossExecPrefix)
                        Text("Opaque hash (hides Boss origin)").tag(EditorialBranchNaming.opaqueHash)
                        Text("Custom prefix").tag(EditorialBranchNaming.customPrefix(prefix: customPrefix))
                    }
                    .labelsHidden()
                    .pickerStyle(.radioGroup)
                    .onChange(of: branchNaming) { _, newVal in
                        if case .customPrefix = newVal, customPrefix.isEmpty {
                            customPrefix = ""
                        }
                    }
                    if case .customPrefix = branchNaming {
                        HStack(spacing: 6) {
                            Text("Prefix:")
                                .font(.callout)
                                .foregroundStyle(.secondary)
                            TextField("e.g. bduff/", text: $customPrefix)
                                .textFieldStyle(.roundedBorder)
                                .frame(maxWidth: 200)
                        }
                        .padding(.leading, 20)
                        Text("Trailing / is conventional. Workers push to <prefix>exec_<id>.")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                            .padding(.leading, 20)
                    }
                }

                // AI co-author trailer
                EditorialSection("Commit Trailers") {
                    Picker("Trailer policy", selection: $trailerPolicy) {
                        Text("Default (follow CLAUDE.md — append AI co-author trailer)").tag(EditorialTrailerPolicy.default)
                        Text("No AI trailer — strip co-author line from commit messages").tag(EditorialTrailerPolicy.noAiTrailer)
                    }
                    .labelsHidden()
                    .pickerStyle(.radioGroup)
                }

                // PR template policy
                EditorialSection("PR Template") {
                    Picker("Template policy", selection: $templatePolicy) {
                        Text("Off — no template enforcement").tag(EditorialTemplatePolicy.off)
                        Text("Advise — inject template as guidance, don't block").tag(EditorialTemplatePolicy.advise)
                        Text("Enforce — block PR bodies that omit mandatory sections").tag(EditorialTemplatePolicy.enforce)
                    }
                    .labelsHidden()
                    .pickerStyle(.radioGroup)
                }

                // Free-text instructions
                EditorialSection("Custom Instructions") {
                    VStack(alignment: .leading, spacing: 6) {
                        TextEditor(text: $instructions)
                            .font(.system(.body, design: .monospaced))
                            .frame(minHeight: 80, maxHeight: 160)
                            .border(Color(nsColor: .separatorColor))
                        Text("Injected verbatim into the worker prompt under an [editorial-rules] header.")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                }

                // Redaction rules
                EditorialSection("Redaction Rules") {
                    RedactionRulesEditor(redactions: $redactions)
                }
            }
            .padding(20)
        }

        Divider()

        HStack {
            Button("Clear All Rules") {
                model.setProductEditorialRules(productID: product.id, rules: nil)
                // Reset local state to defaults
                branchNaming = .bossExecPrefix
                customPrefix = ""
                trailerPolicy = .default
                templatePolicy = .off
                instructions = ""
                redactions = []
            }
            .foregroundStyle(.red)
            .disabled(!model.isConnected)

            Spacer()

            Button("Save") {
                let resolvedNaming: EditorialBranchNaming
                if case .customPrefix = branchNaming {
                    resolvedNaming = .customPrefix(prefix: customPrefix)
                } else {
                    resolvedNaming = branchNaming
                }
                let rules = EditorialRules(
                    instructions: instructions.isEmpty ? nil : instructions,
                    redactions: redactions,
                    templatePolicy: templatePolicy,
                    branchNaming: resolvedNaming,
                    commitTrailerPolicy: trailerPolicy
                )
                model.setProductEditorialRules(productID: product.id, rules: rules)
            }
            .keyboardShortcut("s", modifiers: .command)
            .disabled(!model.isConnected)
        }
        .padding(.horizontal, 20)
        .padding(.vertical, 12)
    }
}

// MARK: - Section wrapper

private struct EditorialSection<Content: View>: View {
    let title: String
    @ViewBuilder let content: () -> Content

    init(_ title: String, @ViewBuilder content: @escaping () -> Content) {
        self.title = title
        self.content = content
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(title)
                .font(.headline)
            content()
        }
    }
}

// MARK: - Redaction rules editor

private struct RedactionRulesEditor: View {
    @Binding var redactions: [EditorialRedactionRule]

    @State private var editingIndex: Int?
    @State private var isAddingNew = false

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            if redactions.isEmpty {
                Text("No redaction rules configured.")
                    .font(.callout)
                    .foregroundStyle(.secondary)
            } else {
                VStack(spacing: 0) {
                    ForEach(Array(redactions.enumerated()), id: \.offset) { index, rule in
                        RedactionRuleRow(
                            rule: rule,
                            onEdit: { editingIndex = index },
                            onDelete: {
                                redactions.remove(at: index)
                            }
                        )
                        if index < redactions.count - 1 {
                            Divider()
                        }
                    }
                }
                .background(Color(nsColor: .controlBackgroundColor))
                .clipShape(RoundedRectangle(cornerRadius: 6))
                .overlay(
                    RoundedRectangle(cornerRadius: 6)
                        .stroke(Color(nsColor: .separatorColor), lineWidth: 0.5)
                )
            }

            Button {
                isAddingNew = true
            } label: {
                Label("Add Rule", systemImage: "plus")
            }
            .buttonStyle(.borderless)

            Text("Patterns use Rust regex syntax. Applied in order to gh pr|issue bodies before the call goes through.")
                .font(.caption)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
        }
        .sheet(isPresented: $isAddingNew) {
            RedactionRuleEditSheet(
                rule: EditorialRedactionRule(pattern: "", replacement: ""),
                title: "Add Redaction Rule",
                onSave: { rule in
                    redactions.append(rule)
                    isAddingNew = false
                },
                onCancel: { isAddingNew = false }
            )
        }
        .sheet(item: Binding(
            get: { editingIndex.map { IndexWrapper($0) } },
            set: { editingIndex = $0?.index }
        )) { wrapper in
            let idx = wrapper.index
            if idx < redactions.count {
                RedactionRuleEditSheet(
                    rule: redactions[idx],
                    title: "Edit Redaction Rule",
                    onSave: { updated in
                        redactions[idx] = updated
                        editingIndex = nil
                    },
                    onCancel: { editingIndex = nil }
                )
            }
        }
    }
}

private struct IndexWrapper: Identifiable {
    let index: Int
    var id: Int { index }
    init(_ index: Int) { self.index = index }
}

private struct RedactionRuleRow: View {
    let rule: EditorialRedactionRule
    let onEdit: () -> Void
    let onDelete: () -> Void

    var body: some View {
        HStack(spacing: 10) {
            VStack(alignment: .leading, spacing: 2) {
                HStack(spacing: 6) {
                    Text(rule.kind == .block ? "Block" : "Rewrite")
                        .font(.system(.caption2, design: .monospaced))
                        .padding(.horizontal, 5)
                        .padding(.vertical, 2)
                        .background(rule.kind == .block ? Color.red.opacity(0.15) : Color.blue.opacity(0.12))
                        .foregroundStyle(rule.kind == .block ? Color.red : Color.blue)
                        .clipShape(RoundedRectangle(cornerRadius: 4))
                    Text(rule.pattern.isEmpty ? "(empty)" : rule.pattern)
                        .font(.system(.callout, design: .monospaced))
                        .lineLimit(1)
                        .truncationMode(.middle)
                }
                if rule.kind == .rewrite {
                    Text("→ \(rule.replacement.isEmpty ? "(empty string)" : rule.replacement)")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                }
            }
            Spacer()
            Button("Edit", action: onEdit)
                .buttonStyle(.borderless)
                .font(.caption)
            Button(role: .destructive, action: onDelete) {
                Image(systemName: "trash")
                    .font(.caption)
            }
            .buttonStyle(.borderless)
            .foregroundStyle(.red)
        }
        .padding(.horizontal, 10)
        .padding(.vertical, 7)
    }
}

private struct RedactionRuleEditSheet: View {
    @State private var pattern: String
    @State private var replacement: String
    @State private var kind: EditorialRedactionKind

    let title: String
    let onSave: (EditorialRedactionRule) -> Void
    let onCancel: () -> Void

    init(
        rule: EditorialRedactionRule,
        title: String,
        onSave: @escaping (EditorialRedactionRule) -> Void,
        onCancel: @escaping () -> Void
    ) {
        _pattern = State(initialValue: rule.pattern)
        _replacement = State(initialValue: rule.replacement)
        _kind = State(initialValue: rule.kind)
        self.title = title
        self.onSave = onSave
        self.onCancel = onCancel
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            Text(title)
                .font(.headline)

            LabeledContent("Action") {
                Picker("Kind", selection: $kind) {
                    Text("Rewrite").tag(EditorialRedactionKind.rewrite)
                    Text("Block").tag(EditorialRedactionKind.block)
                }
                .labelsHidden()
                .pickerStyle(.radioGroup)
            }

            LabeledContent("Pattern") {
                VStack(alignment: .leading, spacing: 4) {
                    TextField("Regex pattern", text: $pattern)
                    Text("Rust regex syntax. Matched against the full PR/issue body.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
            }

            if kind == .rewrite {
                LabeledContent("Replacement") {
                    VStack(alignment: .leading, spacing: 4) {
                        TextField("Replacement text", text: $replacement)
                        Text("Substituted for every match. Leave blank to delete the match.")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                }
            }

            HStack {
                Spacer()
                Button("Cancel", action: onCancel)
                Button("Save") {
                    onSave(EditorialRedactionRule(
                        pattern: pattern,
                        replacement: kind == .block ? "" : replacement,
                        kind: kind
                    ))
                }
                .keyboardShortcut(.defaultAction)
                .disabled(pattern.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
            }
        }
        .padding(24)
        .frame(width: 440)
    }
}

// MARK: - Actions audit pane

private struct EditorialActionsPane: View {
    @ObservedObject var model: ChatViewModel
    let productID: String

    private var actions: [EditorialAction] {
        model.editorialActionsByProductID[productID] ?? []
    }

    private var fetchState: AutomationsFetchState? {
        model.editorialActionsFetchStateByProductID[productID]
    }

    var body: some View {
        VStack(spacing: 0) {
            HStack {
                Spacer()
                Button {
                    model.loadEditorialActions(productID: productID)
                } label: {
                    Label("Refresh", systemImage: "arrow.clockwise")
                }
                .buttonStyle(.borderless)
                .disabled(!model.isConnected)
            }
            .padding(.horizontal, 16)
            .padding(.vertical, 8)

            Divider()

            Group {
                switch fetchState {
                case .none, .loading:
                    if actions.isEmpty {
                        ProgressView()
                            .frame(maxWidth: .infinity, maxHeight: .infinity)
                    } else {
                        actionsList
                    }
                case .failed(let msg):
                    Text("Load failed: \(msg)")
                        .foregroundStyle(.secondary)
                        .frame(maxWidth: .infinity, maxHeight: .infinity)
                case .loaded:
                    if actions.isEmpty {
                        Text("No editorial actions recorded for this product.")
                            .foregroundStyle(.secondary)
                            .frame(maxWidth: .infinity, maxHeight: .infinity)
                    } else {
                        actionsList
                    }
                }
            }
        }
        .onAppear {
            if fetchState == nil {
                model.loadEditorialActions(productID: productID)
            }
        }
    }

    private var actionsList: some View {
        Table(actions) {
            TableColumn("Action") { action in
                Text(action.action.capitalized)
                    .font(.callout)
                    .foregroundStyle(actionColor(action.action))
            }
            .width(60)

            TableColumn("Reason") { action in
                Text(action.reason)
                    .font(.callout)
                    .lineLimit(2)
            }

            TableColumn("Short ID") { action in
                Text(action.executionID.prefix(8))
                    .font(.system(.caption, design: .monospaced))
                    .foregroundStyle(.secondary)
            }
            .width(70)

            TableColumn("When") { action in
                Text(action.createdAt.prefix(16).replacingOccurrences(of: "T", with: " "))
                    .font(.system(.caption, design: .monospaced))
                    .foregroundStyle(.secondary)
            }
            .width(130)
        }
    }

    private func actionColor(_ action: String) -> Color {
        switch action {
        case "block": return .red
        case "rewrite": return .orange
        case "advise": return .blue
        default: return .primary
        }
    }
}
