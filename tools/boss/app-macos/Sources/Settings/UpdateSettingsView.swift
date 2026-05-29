import SwiftUI
import UpdateCore

/// "Updates" tab in the Boss Settings window.
///
/// Shows the update mode picker, current version, last-checked time, a
/// manual-check button, and the staged-update status line.
/// Reads and writes through `UpdateModel`; bypasses engine RPC entirely.
struct UpdateSettingsView: View {
    @ObservedObject var model: UpdateModel

    var body: some View {
        Form {
            Section {
                modePicker
            } header: {
                Text("Update Mode")
            }

            Section {
                LabeledContent("Current version", value: currentVersion)
                LabeledContent("Last checked", value: lastCheckedLabel)
                HStack(spacing: 8) {
                    Button("Check Now") {
                        Task { await model.checkNow() }
                    }
                    .disabled(model.isChecking)
                    if model.isChecking {
                        ProgressView()
                            .controlSize(.small)
                        Text("Checking…")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                }
            } header: {
                Text("Status")
            }

            if let status = statusLine {
                Section {
                    HStack(spacing: 6) {
                        Image(systemName: statusIcon)
                            .foregroundStyle(statusColor)
                        Text(status)
                    }
                }
            }
        }
        .formStyle(.grouped)
        .padding()
    }

    // MARK: - Mode picker

    private var modePicker: some View {
        VStack(alignment: .leading, spacing: 8) {
            Picker("", selection: Binding(
                get: { model.mode },
                set: { model.setMode($0) }
            )) {
                Text("Manual only").tag(UpdateMode.manual)
                Text("Notify").tag(UpdateMode.notify)
                Text("Automatic").tag(UpdateMode.automatic)
            }
            .pickerStyle(.segmented)
            .labelsHidden()
            Text(modeDescription)
                .font(.caption)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
        }
        .padding(.vertical, 2)
    }

    private var modeDescription: String {
        switch model.mode {
        case .manual:
            return "No automatic polling. Use \"Check for Updates…\" in the Boss menu to check on demand."
        case .notify:
            return "Polls every 6 hours and shows a badge in the toolbar when an update is available."
        case .automatic:
            return "Polls every 6 hours. Downloads and installs updates automatically at the next safe boundary (quit or startup)."
        }
    }

    // MARK: - Status rows

    private var currentVersion: String {
        Bundle.main.object(forInfoDictionaryKey: "BossFullVersion") as? String
            ?? Bundle.main.infoDictionary?["CFBundleShortVersionString"] as? String
            ?? "Unknown"
    }

    private var lastCheckedLabel: String {
        guard let date = model.lastCheckDate else { return "Never" }
        let formatter = RelativeDateTimeFormatter()
        formatter.unitsStyle = .full
        return formatter.localizedString(for: date, relativeTo: Date())
    }

    // MARK: - Staged-status line

    private var statusLine: String? {
        guard let result = model.lastCheckResult else { return nil }
        switch result {
        case .upToDate:
            return "Boss is up to date."
        case .available(let update):
            return "Version \(update.version) is available."
        case .rateLimited(let retryAfter):
            let formatter = DateFormatter()
            formatter.timeStyle = .short
            formatter.dateStyle = .none
            return "Rate-limited by GitHub. Try again after \(formatter.string(from: retryAfter))."
        case .networkError(let message):
            return "Check failed: \(message)"
        }
    }

    private var statusIcon: String {
        switch model.lastCheckResult {
        case .upToDate:
            return "checkmark.circle.fill"
        case .available:
            return "arrow.down.circle.fill"
        case .rateLimited, .networkError:
            return "exclamationmark.triangle.fill"
        case nil:
            return "circle"
        }
    }

    private var statusColor: Color {
        switch model.lastCheckResult {
        case .upToDate:
            return .green
        case .available:
            return .accentColor
        case .rateLimited, .networkError:
            return .orange
        case nil:
            return .secondary
        }
    }
}
