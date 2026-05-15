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
            FeatureFlagsViewer()
                .tabItem {
                    Label("Feature Flags", systemImage: "flag")
                }
        }
        .environmentObject(chatModel)
        .onAppear {
            chatModel.refreshSettings()
        }
        .frame(minWidth: 560, minHeight: 360)
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
