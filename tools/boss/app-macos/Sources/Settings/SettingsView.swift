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

/// "Workers" pane — PR-related worker defaults.
private struct WorkerSettingsPane: View {
    @EnvironmentObject private var chatModel: ChatViewModel

    var body: some View {
        Form {
            Section {
                if chatModel.engineSettings.isEmpty {
                    ProgressView("Loading…")
                        .frame(maxWidth: .infinity, alignment: .center)
                        .padding()
                } else {
                    ForEach(chatModel.engineSettings) { setting in
                        SettingToggleRow(setting: setting) { enabled in
                            chatModel.setEngineSetting(key: setting.key, enabled: enabled)
                        }
                    }
                }
            } header: {
                Text("PR Conventions")
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
