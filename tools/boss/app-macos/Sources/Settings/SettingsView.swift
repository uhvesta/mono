import SwiftUI

/// macOS Settings window for Boss (opened via Boss → Settings… or ⌘,).
///
/// Reads current values from the engine at appear time and writes back
/// through `SetSetting` RPCs so settings live in engine state rather
/// than `UserDefaults`. Different machines each carry their own
/// `state.db` and therefore their own independent settings.
struct SettingsView: View {
    @EnvironmentObject private var chatModel: ChatViewModel

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
/// glance the engine sees the env var.
private struct EngineConfigPane: View {
    @EnvironmentObject private var chatModel: ChatViewModel

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
                    Text(chatModel.engineAnthropicApiKeyPresent ? "Detected" : "Not set")
                        .foregroundStyle(.secondary)
                }
                if !chatModel.engineAnthropicApiKeyPresent {
                    Text("Live worker summaries and pane summarization are disabled until ANTHROPIC_API_KEY is exported in the environment Boss launches its engine from. Set the variable in your shell startup file, then quit and relaunch Boss.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }
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
