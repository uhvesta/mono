import SwiftUI
import UpdateCore

/// macOS Settings window for Boss (opened via Boss → Settings… or ⌘,).
///
/// Reads current values from the engine at appear time and writes back
/// through `SetSetting` RPCs so settings live in engine state rather
/// than `UserDefaults`. Different machines each carry their own
/// `state.db` and therefore their own independent settings.
struct SettingsView: View {
    @EnvironmentObject private var chatModel: ChatViewModel
    @EnvironmentObject private var updateModel: UpdateModel

    var body: some View {
        TabView {
            WorkerSettingsPane()
                .tabItem {
                    Label("Workers", systemImage: "person.2")
                }
            EngineConfigPane()
                .tabItem {
                    Label("Engine", systemImage: "gearshape")
                }
            FeatureFlagsViewer()
                .tabItem {
                    Label("Feature Flags", systemImage: "flag")
                }
            UpdateSettingsView(model: updateModel)
                .tabItem {
                    Label("Updates", systemImage: "arrow.down.circle")
                }
        }
        .environmentObject(chatModel)
        .onAppear {
            chatModel.refreshSettings()
            // Engine health is fetched on every reconnect, but the
            // user may open Settings against a long-lived session
            // where the API-key state changed (a restart with a new
            // env var). Re-poll on appear so the pane shows the
            // current truth, not a snapshot from minutes ago.
            chatModel.refreshEngineHealth()
        }
        .frame(minWidth: 560, minHeight: 360)
    }
}

/// "Engine" pane — engine-side configuration health.
/// Renders the same issues the chrome banner shows, plus the raw
/// `ANTHROPIC_API_KEY` presence bit so the user can confirm at a
/// glance the engine sees the env var. Also surfaces the
/// Keychain-backed override added in #735 so launching from
/// Finder/Spotlight no longer requires a launchd plist or shell-
/// inherited env.
private struct EngineConfigPane: View {
    @EnvironmentObject private var chatModel: ChatViewModel

    /// SecureField draft — never persisted, never inspected by other
    /// state. Cleared on save so a typed-then-cancelled value doesn't
    /// linger in memory longer than the pane is open.
    @State private var apiKeyDraft: String = ""

    /// Mirror of `APIKeyStore.readAnthropicApiKey() != nil` so the UI
    /// can render "stored" / "not stored" without re-querying the
    /// Keychain on every redraw. Refreshed on appear and after every
    /// save / clear.
    @State private var hasStoredApiKey: Bool = APIKeyStore.readAnthropicApiKey() != nil

    /// User-visible error message from the last save / clear attempt.
    /// `nil` means the last action succeeded (or none has happened).
    @State private var apiKeyError: String?

    /// Transient status line shown after a successful save / clear.
    @State private var apiKeyStatus: String?

    var body: some View {
        Form {
            Section {
                HStack(spacing: 6) {
                    Image(systemName: chatModel.engineAnthropicApiKeyPresent
                          ? "checkmark.circle.fill"
                          : "exclamationmark.triangle.fill")
                        .foregroundStyle(chatModel.engineAnthropicApiKeyPresent ? .green : .orange)
                    Text("ANTHROPIC_API_KEY")
                        .font(.body.weight(.medium))
                    Spacer()
                    Text(engineKeyStatusLabel)
                        .foregroundStyle(.secondary)
                }
                if !chatModel.engineAnthropicApiKeyPresent {
                    Text("Live worker summaries and pane summarization are disabled until ANTHROPIC_API_KEY is available to the engine. Paste a key below to store it in the macOS Keychain, or export the variable in your shell startup file and relaunch Boss.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }

                VStack(alignment: .leading, spacing: 8) {
                    SecureField("sk-ant-…", text: $apiKeyDraft)
                        .textFieldStyle(.roundedBorder)
                        .disabled(chatModel.isRestartingEngine)
                    HStack(spacing: 8) {
                        Button(hasStoredApiKey ? "Save & restart engine" : "Save") {
                            saveApiKey()
                        }
                        .disabled(apiKeyDraft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
                                  || chatModel.isRestartingEngine)
                        if hasStoredApiKey {
                            Button("Clear stored key") {
                                clearApiKey()
                            }
                            .disabled(chatModel.isRestartingEngine)
                        }
                        if chatModel.isRestartingEngine {
                            ProgressView()
                                .controlSize(.small)
                            Text("Restarting engine…")
                                .font(.caption)
                                .foregroundStyle(.secondary)
                        }
                    }
                    Text("Stored in the macOS Keychain on signed release builds, or a private file under Application Support on ad-hoc dev builds (service \(APIKeyStore.service)). The Settings value overrides any ANTHROPIC_API_KEY in the engine's inherited environment. Saving restarts the engine so the new value takes effect.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                    if let apiKeyError {
                        Text(apiKeyError)
                            .font(.caption)
                            .foregroundStyle(.red)
                            .fixedSize(horizontal: false, vertical: true)
                    } else if let apiKeyStatus {
                        Text(apiKeyStatus)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                }
                .padding(.top, 4)
            } header: {
                Text("Required Configuration")
            }

            if !chatModel.engineHealthIssues.isEmpty {
                Section {
                    ForEach(chatModel.engineHealthIssues) { issue in
                        VStack(alignment: .leading, spacing: 4) {
                            HStack(spacing: 6) {
                                Image(systemName: issue.severity == "error"
                                      ? "exclamationmark.octagon.fill"
                                      : "exclamationmark.triangle.fill")
                                    .foregroundStyle(issue.severity == "error" ? .red : .orange)
                                Text(issue.title)
                                    .font(.body.weight(.medium))
                            }
                            Text(issue.body)
                                .font(.caption)
                                .foregroundStyle(.secondary)
                                .fixedSize(horizontal: false, vertical: true)
                        }
                        .padding(.vertical, 2)
                    }
                } header: {
                    Text("Health Issues")
                }
            }
        }
        .formStyle(.grouped)
        .padding()
        .onAppear {
            // Refresh the Keychain-backed state on every appear so the
            // "Stored" indicator reflects edits that happened outside
            // this pane (another session, manual Keychain Access edit).
            hasStoredApiKey = APIKeyStore.readAnthropicApiKey() != nil
        }
    }

    /// Combined label for the presence row. Distinguishes "the engine
    /// currently sees a key" (the runtime truth) from "we have a key
    /// stored that will be applied at the next engine launch" (the
    /// Settings truth). The two diverge for a brief window after Save
    /// while the engine is restarting.
    private var engineKeyStatusLabel: String {
        if chatModel.engineAnthropicApiKeyPresent {
            return hasStoredApiKey ? "Detected (from Settings)" : "Detected"
        }
        return hasStoredApiKey ? "Stored — restart engine to apply" : "Not set"
    }

    private func saveApiKey() {
        let trimmed = apiKeyDraft.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else {
            apiKeyError = "API key cannot be empty."
            apiKeyStatus = nil
            return
        }
        do {
            try APIKeyStore.saveAnthropicApiKey(trimmed)
            apiKeyDraft = ""
            hasStoredApiKey = true
            apiKeyError = nil
            apiKeyStatus = "Saved. Restarting engine to apply…"
            // Bounce the engine so the freshly-stored key is injected
            // into its env on the next spawn (see
            // EngineProcessController.launchDetached).
            chatModel.restartEngine()
        } catch {
            apiKeyError = (error as? LocalizedError)?.errorDescription
                ?? error.localizedDescription
            apiKeyStatus = nil
        }
    }

    private func clearApiKey() {
        do {
            try APIKeyStore.clearAnthropicApiKey()
            hasStoredApiKey = false
            apiKeyError = nil
            apiKeyStatus = "Cleared. Restarting engine so summarization falls back to env / disabled."
            chatModel.restartEngine()
        } catch {
            apiKeyError = (error as? LocalizedError)?.errorDescription
                ?? error.localizedDescription
            apiKeyStatus = nil
        }
    }
}

/// "Workers" pane — worker defaults grouped by concern.
private struct WorkerSettingsPane: View {
    @EnvironmentObject private var chatModel: ChatViewModel

    private var prSettings: [EngineSetting] {
        chatModel.engineSettings.filter { $0.key == "default_pr_draft_mode" }
    }

    private var permissionModeSetting: EngineSetting? {
        chatModel.engineSettings.first { $0.key == "workers.non_opus_permission_mode" }
    }

    private var coordinatorSettings: [EngineSetting] {
        chatModel.engineSettings.filter { $0.key == "coordinator.direct_developer_mode" }
    }

    var body: some View {
        Form {
            if chatModel.engineSettings.isEmpty {
                Section {
                    ProgressView("Loading…")
                        .frame(maxWidth: .infinity, alignment: .center)
                        .padding()
                }
            } else {
                Section {
                    ForEach(prSettings) { setting in
                        SettingToggleRow(setting: setting) { enabled in
                            chatModel.setEngineSetting(key: setting.key, enabled: enabled)
                        }
                    }
                } header: {
                    Text("PR Conventions")
                }
                if let setting = permissionModeSetting {
                    Section {
                        PermissionModePickerRow(setting: setting) { enabled in
                            chatModel.setEngineSetting(key: setting.key, enabled: enabled)
                        }
                    } header: {
                        Text("Workers")
                    }
                }
                if !coordinatorSettings.isEmpty {
                    Section {
                        ForEach(coordinatorSettings) { setting in
                            SettingToggleRow(setting: setting) { enabled in
                                chatModel.setEngineSetting(key: setting.key, enabled: enabled)
                            }
                        }
                    } header: {
                        Text("Coordinator")
                    }
                }
            }
        }
        .formStyle(.grouped)
        .padding()
    }
}

private struct SettingToggleRow: View {
    let setting: EngineSetting
    let onToggle: (Bool) -> Void

    var body: some View {
        Toggle(isOn: Binding(
            get: { setting.enabled },
            set: { onToggle($0) }
        )) {
            VStack(alignment: .leading, spacing: 3) {
                Text(labelText(for: setting.key))
                    .font(.body)
                Text(setting.description)
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }
        .toggleStyle(.switch)
        .padding(.vertical, 2)
    }

    private func labelText(for key: String) -> String {
        switch key {
        case "default_pr_draft_mode":
            return "Default new PRs to draft mode"
        case "coordinator.direct_developer_mode":
            return "Direct Boss developer mode"
        default:
            return key
        }
    }
}

/// Segmented picker for the two-value `workers.non_opus_permission_mode` setting.
/// `false` (default) = --dangerously-skip-permissions (personal laptop).
/// `true` = --permission-mode auto (corp laptop).
private struct PermissionModePickerRow: View {
    let setting: EngineSetting
    let onChange: (Bool) -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text("Permission mode for Sonnet/Haiku workers")
                .font(.body)
            Text(setting.description)
                .font(.caption)
                .foregroundStyle(.secondary)
            Picker("", selection: Binding(
                get: { setting.enabled },
                set: { onChange($0) }
            )) {
                Text("Skip permissions (default)").tag(false)
                Text("Auto mode").tag(true)
            }
            .pickerStyle(.segmented)
            .labelsHidden()
        }
        .padding(.vertical, 2)
    }
}
