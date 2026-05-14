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
        }
        .environmentObject(chatModel)
        .onAppear {
            chatModel.refreshSettings()
        }
        .frame(minWidth: 480, minHeight: 260)
    }
}

/// "Workers" pane — worker defaults grouped by concern.
private struct WorkerSettingsPane: View {
    @EnvironmentObject private var chatModel: ChatViewModel

    private var prSettings: [EngineSetting] {
        chatModel.engineSettings.filter { $0.key == "default_pr_draft_mode" }
    }

    private var modelSettings: [EngineSetting] {
        chatModel.engineSettings.filter { $0.key == "workers.always_use_opus" }
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
                Section {
                    ForEach(modelSettings) { setting in
                        SettingToggleRow(setting: setting) { enabled in
                            chatModel.setEngineSetting(key: setting.key, enabled: enabled)
                        }
                    }
                } header: {
                    Text("Model")
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
        case "workers.always_use_opus":
            return "Always use Opus for workers"
        default:
            return key
        }
    }
}
