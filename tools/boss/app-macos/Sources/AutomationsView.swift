import SwiftUI

// MARK: - Top-level Automations tab

struct AutomationsView: View {
    @ObservedObject var model: ChatViewModel

    var body: some View {
        NavigationSplitView {
            AutomationsSidebar(model: model)
                .navigationSplitViewColumnWidth(min: 240, ideal: 300, max: 400)
        } detail: {
            if let automation = model.selectedAutomation {
                AutomationDetailView(model: model, automation: automation)
            } else {
                AutomationsEmptyState(model: model)
            }
        }
    }
}

// MARK: - Sidebar

private struct AutomationsSidebar: View {
    @ObservedObject var model: ChatViewModel
    @State private var isCreating = false

    var body: some View {
        VStack(spacing: 0) {
            List(selection: Binding(
                get: { model.selectedAutomationID },
                set: { model.selectedAutomationID = $0 }
            )) {
                if model.automationsForSelectedProduct.isEmpty {
                    Text("No automations")
                        .foregroundStyle(.secondary)
                        .font(.callout)
                        .listRowBackground(Color.clear)
                } else {
                    ForEach(model.automationsForSelectedProduct) { automation in
                        AutomationRowView(
                            automation: automation,
                            openCount: model.openTaskCountByAutomationID[automation.id]
                        )
                        .tag(automation.id)
                    }
                }
            }
            .listStyle(.sidebar)

            Divider()

            HStack {
                Button {
                    model.refreshAutomations()
                } label: {
                    Label("Refresh", systemImage: "arrow.clockwise")
                }
                .buttonStyle(.borderless)
                Spacer()
                Button {
                    isCreating = true
                } label: {
                    Image(systemName: "plus")
                }
                .buttonStyle(.borderless)
                .disabled(model.selectedProduct == nil || !model.isConnected)
                .help("New Automation")
            }
            .padding(.horizontal, 12)
            .padding(.vertical, 8)
        }
        .navigationTitle("Automations")
        .sheet(isPresented: $isCreating) {
            if let product = model.selectedProduct {
                AutomationEditSheet(
                    mode: .create(productID: product.id),
                    onSave: { name, cron, tz, instruction, limit, enabled, repo in
                        model.createAutomation(
                            productID: product.id,
                            name: name,
                            cron: cron,
                            timezone: tz,
                            standingInstruction: instruction,
                            openTaskLimit: limit,
                            enabled: enabled,
                            repoRemoteURL: repo
                        )
                        isCreating = false
                    },
                    onCancel: { isCreating = false }
                )
            }
        }
    }
}

// MARK: - Automation list row

private struct AutomationRowView: View {
    let automation: AppAutomation
    let openCount: Int?

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack(alignment: .firstTextBaseline, spacing: 6) {
                if let shortID = automation.shortID {
                    Text("A\(shortID)")
                        .font(.system(.caption2, design: .monospaced))
                        .foregroundStyle(.secondary)
                }
                Text(automation.name)
                    .font(.body)
                    .lineLimit(1)
                Spacer(minLength: 4)
                Circle()
                    .fill(automation.enabled ? Color.green : Color.secondary)
                    .frame(width: 7, height: 7)
                    .help(automation.enabled ? "Enabled" : "Disabled")
            }

            Text(automation.trigger.humanReadable)
                .font(.caption)
                .foregroundStyle(.secondary)
                .lineLimit(1)

            HStack(spacing: 6) {
                if let outcome = automation.lastOutcomeLabel {
                    Text(outcome)
                        .font(.caption2)
                        .foregroundStyle(outcomeColor(for: automation.lastOutcome))
                }
                Spacer(minLength: 0)
                if let open = openCount {
                    Text("\(open)/\(automation.openTaskLimit)")
                        .font(.system(.caption2, design: .monospaced))
                        .foregroundStyle(.secondary)
                        .help("Open tasks / limit")
                }
            }
        }
        .padding(.vertical, 2)
    }

    private func outcomeColor(for outcome: String?) -> Color {
        switch outcome {
        case "produced_task": return .green
        case "skipped": return .secondary
        case "suppressed_at_limit": return .orange
        case "failed_will_retry", "failed_gave_up": return .red
        default: return .secondary
        }
    }
}

// MARK: - Empty state

private struct AutomationsEmptyState: View {
    @ObservedObject var model: ChatViewModel

    var body: some View {
        if model.selectedProduct == nil {
            VStack(spacing: 8) {
                Text("Select a product")
                    .font(.title3.weight(.semibold))
                Text("Choose a product from the Work tab first.")
                    .foregroundStyle(.secondary)
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        } else if model.automationsForSelectedProduct.isEmpty {
            VStack(spacing: 8) {
                Image(systemName: "clock.badge.checkmark")
                    .font(.system(size: 36))
                    .foregroundStyle(.secondary)
                Text("No automations yet")
                    .font(.title3.weight(.semibold))
                Text("Automations run on a schedule to check for maintenance work and spawn tasks automatically.")
                    .multilineTextAlignment(.center)
                    .foregroundStyle(.secondary)
                    .frame(maxWidth: 360)
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            .padding(24)
        } else {
            Text("Select an automation")
                .foregroundStyle(.secondary)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
    }
}

// MARK: - Detail view

private struct AutomationDetailView: View {
    @ObservedObject var model: ChatViewModel
    let automation: AppAutomation

    @State private var isEditing = false
    @State private var showDeleteConfirmation = false

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 20) {
                // Header
                HStack(alignment: .firstTextBaseline, spacing: 8) {
                    if let shortID = automation.shortID {
                        Text("A\(shortID)")
                            .font(.system(.body, design: .monospaced))
                            .foregroundStyle(.secondary)
                    }
                    Text(automation.name)
                        .font(.title2.weight(.semibold))
                    Spacer(minLength: 8)
                    Toggle("", isOn: Binding(
                        get: { automation.enabled },
                        set: { enabled in
                            if enabled {
                                model.enableAutomation(id: automation.id)
                            } else {
                                model.disableAutomation(id: automation.id)
                            }
                        }
                    ))
                    .labelsHidden()
                    .help(automation.enabled ? "Disable automation" : "Enable automation")
                    .disabled(!model.isConnected)
                }

                Divider()

                // Schedule
                AutomationDetailSection(title: "Schedule") {
                    LabeledContent("Schedule", value: automation.trigger.humanReadable)
                    LabeledContent("Cron", value: automation.trigger.cronExpression)
                    LabeledContent("Timezone", value: automation.trigger.timezone)
                }

                // Status
                AutomationDetailSection(title: "Status") {
                    LabeledContent("Enabled") {
                        Text(automation.enabled ? "Yes" : "No")
                            .foregroundStyle(automation.enabled ? .primary : .secondary)
                    }
                    if let outcome = automation.lastOutcomeLabel {
                        LabeledContent("Last outcome", value: outcome)
                    }
                    if let nextDue = automation.nextDueAt {
                        LabeledContent("Next fire", value: formatTimestamp(nextDue))
                    } else if automation.enabled {
                        LabeledContent("Next fire", value: "Pending")
                    }
                    if let lastFired = automation.lastFiredAt {
                        LabeledContent("Last fired", value: formatTimestamp(lastFired))
                    }
                    let openCount = model.openTaskCountByAutomationID[automation.id] ?? 0
                    LabeledContent("Open tasks", value: "\(openCount) / \(automation.openTaskLimit)")
                }

                // Instruction
                AutomationDetailSection(title: "Standing Instruction") {
                    Text(automation.standingInstruction)
                        .font(.callout)
                        .foregroundStyle(.secondary)
                        .textSelection(.enabled)
                        .fixedSize(horizontal: false, vertical: true)
                }

                // Settings
                AutomationDetailSection(title: "Settings") {
                    LabeledContent("Open task limit", value: "\(automation.openTaskLimit)")
                    if let repo = automation.repoRemoteURL {
                        LabeledContent("Repo", value: repo)
                    } else {
                        LabeledContent("Repo", value: "Product default")
                    }
                    if let windowSecs = automation.catchUpWindowSecs {
                        LabeledContent("Catch-up window", value: "\(windowSecs)s")
                    }
                }

                // Actions
                HStack(spacing: 12) {
                    Button("Edit") {
                        isEditing = true
                    }
                    .disabled(!model.isConnected)

                    Button("Delete…", role: .destructive) {
                        showDeleteConfirmation = true
                    }
                    .disabled(!model.isConnected)
                }
                .padding(.top, 4)
            }
            .padding(20)
            .frame(maxWidth: .infinity, alignment: .topLeading)
        }
        .sheet(isPresented: $isEditing) {
            AutomationEditSheet(
                mode: .edit(automation: automation),
                onSave: { name, cron, tz, instruction, limit, _, repo in
                    model.updateAutomation(
                        id: automation.id,
                        name: name,
                        cron: cron,
                        timezone: tz,
                        standingInstruction: instruction,
                        openTaskLimit: limit
                    )
                    isEditing = false
                },
                onCancel: { isEditing = false }
            )
        }
        .confirmationDialog(
            "Delete Automation",
            isPresented: $showDeleteConfirmation,
            titleVisibility: .visible
        ) {
            Button("Delete \"\(automation.name)\"", role: .destructive) {
                model.deleteAutomation(id: automation.id)
            }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text("This will permanently delete the automation and its run history. Tasks already produced by this automation are not affected.")
        }
    }

    private func formatTimestamp(_ iso: String) -> String {
        let formatter = ISO8601DateFormatter()
        formatter.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        if let date = formatter.date(from: iso) {
            let rel = RelativeDateTimeFormatter()
            rel.unitsStyle = .full
            return rel.localizedString(for: date, relativeTo: Date())
        }
        return iso
    }
}

private struct AutomationDetailSection<Content: View>: View {
    let title: String
    @ViewBuilder let content: () -> Content

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(title)
                .font(.subheadline.weight(.semibold))
                .foregroundStyle(.secondary)
            content()
        }
    }
}

// MARK: - Create / Edit sheet

enum AutomationEditMode {
    case create(productID: String)
    case edit(automation: AppAutomation)
}

struct AutomationEditSheet: View {
    let mode: AutomationEditMode
    let onSave: (String, String, String, String, Int, Bool, String?) -> Void
    let onCancel: () -> Void

    @State private var name: String = ""
    @State private var selectedPreset: SchedulePreset = .weekdayAfternoon
    @State private var customCron: String = ""
    @State private var timezone: String = TimeZone.current.identifier
    @State private var standingInstruction: String = ""
    @State private var openTaskLimit: Int = 1
    @State private var enabled: Bool = true
    @State private var repoRemoteURL: String = ""

    private var isEdit: Bool {
        if case .edit = mode { return true }
        return false
    }

    private var effectiveCron: String {
        if selectedPreset == .custom {
            return customCron.trimmingCharacters(in: .whitespacesAndNewlines)
        }
        return selectedPreset.cronExpression ?? ""
    }

    private var isValid: Bool {
        let trimmedName = name.trimmingCharacters(in: .whitespacesAndNewlines)
        let cron = effectiveCron
        let trimmedInstruction = standingInstruction.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmedName.isEmpty, !trimmedInstruction.isEmpty, !timezone.isEmpty else { return false }
        // Basic cron validation: 5 whitespace-separated fields
        let fields = cron.split(separator: " ", omittingEmptySubsequences: true)
        return fields.count == 5
    }

    var body: some View {
        VStack(spacing: 0) {
            // Header
            HStack {
                Text(isEdit ? "Edit Automation" : "New Automation")
                    .font(.headline)
                Spacer()
            }
            .padding(.horizontal, 20)
            .padding(.top, 20)
            .padding(.bottom, 16)

            Divider()

            ScrollView {
                VStack(alignment: .leading, spacing: 20) {
                    // Name
                    VStack(alignment: .leading, spacing: 6) {
                        Text("Name")
                            .font(.subheadline.weight(.semibold))
                        TextField("e.g. Fix clippy warnings", text: $name)
                            .textFieldStyle(.roundedBorder)
                    }

                    // Schedule
                    VStack(alignment: .leading, spacing: 8) {
                        Text("Schedule")
                            .font(.subheadline.weight(.semibold))

                        Picker("Preset", selection: $selectedPreset) {
                            ForEach(SchedulePreset.allCases) { preset in
                                Text(preset.label).tag(preset)
                            }
                        }
                        .labelsHidden()

                        if selectedPreset == .custom {
                            TextField("Cron expression (e.g. 0 14 * * 1-5)", text: $customCron)
                                .textFieldStyle(.roundedBorder)
                                .font(.system(.body, design: .monospaced))
                        } else {
                            Text(effectiveCron)
                                .font(.system(.caption, design: .monospaced))
                                .foregroundStyle(.secondary)
                                .padding(.horizontal, 8)
                                .padding(.vertical, 4)
                                .background(Color(nsColor: .quaternaryLabelColor).opacity(0.15))
                                .clipShape(RoundedRectangle(cornerRadius: 4))
                        }

                        VStack(alignment: .leading, spacing: 4) {
                            Text("Timezone")
                                .font(.caption)
                                .foregroundStyle(.secondary)
                            TimezonePicker(selection: $timezone)
                        }
                    }

                    // Standing instruction
                    VStack(alignment: .leading, spacing: 6) {
                        Text("Standing Instruction")
                            .font(.subheadline.weight(.semibold))
                        Text("Describe the recurring maintenance task for the triage agent.")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                        TextEditor(text: $standingInstruction)
                            .font(.callout)
                            .frame(minHeight: 100, maxHeight: 200)
                            .overlay(
                                RoundedRectangle(cornerRadius: 6)
                                    .stroke(Color(nsColor: .separatorColor), lineWidth: 1)
                            )
                    }

                    // Settings
                    VStack(alignment: .leading, spacing: 10) {
                        Text("Settings")
                            .font(.subheadline.weight(.semibold))

                        HStack {
                            Text("Open task limit")
                                .font(.callout)
                            Spacer()
                            Stepper("\(openTaskLimit)", value: $openTaskLimit, in: 1...10)
                        }

                        if !isEdit {
                            Toggle("Start enabled", isOn: $enabled)
                        }

                        VStack(alignment: .leading, spacing: 4) {
                            Text("Repo (optional)")
                                .font(.caption)
                                .foregroundStyle(.secondary)
                            TextField("Repo remote URL — leave blank to use product default", text: $repoRemoteURL)
                                .textFieldStyle(.roundedBorder)
                                .font(.callout)
                        }
                    }
                }
                .padding(20)
            }

            Divider()

            HStack {
                Button("Cancel", role: .cancel) { onCancel() }
                Spacer()
                Button(isEdit ? "Save" : "Create") {
                    let repoArg = repoRemoteURL.trimmingCharacters(in: .whitespacesAndNewlines)
                    onSave(
                        name.trimmingCharacters(in: .whitespacesAndNewlines),
                        effectiveCron,
                        timezone,
                        standingInstruction.trimmingCharacters(in: .whitespacesAndNewlines),
                        openTaskLimit,
                        enabled,
                        repoArg.isEmpty ? nil : repoArg
                    )
                }
                .buttonStyle(.borderedProminent)
                .disabled(!isValid)
            }
            .padding(.horizontal, 20)
            .padding(.vertical, 16)
        }
        .frame(width: 520)
        .onAppear { populateFromMode() }
    }

    private func populateFromMode() {
        guard case .edit(let automation) = mode else { return }
        name = automation.name
        standingInstruction = automation.standingInstruction
        openTaskLimit = automation.openTaskLimit
        enabled = automation.enabled
        repoRemoteURL = automation.repoRemoteURL ?? ""
        timezone = automation.trigger.timezone
        let cron = automation.trigger.cronExpression
        if let preset = SchedulePreset.preset(forCron: cron) {
            selectedPreset = preset
        } else {
            selectedPreset = .custom
            customCron = cron
        }
    }
}

// MARK: - Timezone picker

private struct TimezonePicker: View {
    @Binding var selection: String

    private let commonZones: [String] = {
        let preferred = [
            "America/Los_Angeles",
            "America/Denver",
            "America/Chicago",
            "America/New_York",
            "America/Sao_Paulo",
            "Europe/London",
            "Europe/Paris",
            "Europe/Berlin",
            "Asia/Tokyo",
            "Asia/Shanghai",
            "Asia/Kolkata",
            "Australia/Sydney",
            "UTC",
        ]
        let system = TimeZone.current.identifier
        var all = preferred
        if !all.contains(system) { all.insert(system, at: 0) }
        return all
    }()

    var body: some View {
        Picker("Timezone", selection: $selection) {
            ForEach(commonZones, id: \.self) { tz in
                Text(tz).tag(tz)
            }
            Divider()
            ForEach(otherZones, id: \.self) { tz in
                Text(tz).tag(tz)
            }
        }
        .labelsHidden()
    }

    private var otherZones: [String] {
        TimeZone.knownTimeZoneIdentifiers
            .filter { !commonZones.contains($0) }
            .sorted()
    }
}
