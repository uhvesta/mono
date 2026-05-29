import AppKit
import SwiftUI
import Textual
import UpdateCore

/// Sheet shown by "Check for Updates…" and (in a later task) by the chrome badge.
/// Driven by `UpdateModel`; all state transitions happen there.
struct UpdateResultSheet: View {
    @EnvironmentObject private var updateModel: UpdateModel
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        content
            .padding(24)
            .frame(minWidth: 480, maxWidth: 560)
    }

    // MARK: - State dispatch

    @ViewBuilder
    private var content: some View {
        if updateModel.isChecking {
            checkingView
        } else {
            switch updateModel.lastCheckResult {
            case nil:
                checkingView
            case .upToDate:
                upToDateView
            case .available(let update):
                availableView(update: update)
            case .rateLimited(let retryAfter):
                rateLimitedView(retryAfter: retryAfter)
            case .networkError(let message):
                errorView(message: message)
            }
        }
    }

    // MARK: - Checking state

    private var checkingView: some View {
        VStack(alignment: .leading, spacing: 20) {
            HStack(spacing: 12) {
                ProgressView()
                    .controlSize(.regular)
                Text("Checking for updates…")
                    .font(.title2.weight(.semibold))
            }
            Divider()
            HStack {
                Spacer()
                Button("Cancel") { dismiss() }
                    .keyboardShortcut(.cancelAction)
            }
        }
    }

    // MARK: - Up to date

    private var upToDateView: some View {
        let version = Bundle.main.infoDictionary?["CFBundleShortVersionString"] as? String ?? "?"
        return VStack(alignment: .leading, spacing: 20) {
            HStack(alignment: .top, spacing: 12) {
                Image(systemName: "checkmark.circle.fill")
                    .font(.largeTitle)
                    .foregroundStyle(.green)
                VStack(alignment: .leading, spacing: 4) {
                    Text("Boss is up to date")
                        .font(.title2.weight(.semibold))
                    Text("Boss \(version) is the latest version.")
                        .foregroundStyle(.secondary)
                }
            }
            Divider()
            HStack {
                Spacer()
                Button("OK") { dismiss() }
                    .keyboardShortcut(.defaultAction)
            }
        }
    }

    // MARK: - Update available

    @ViewBuilder
    private func availableView(update: AvailableUpdate) -> some View {
        let isDevBuild = updateModel.isDevBuild
        let fullVersion = Bundle.main.infoDictionary?["BossFullVersion"] as? String

        VStack(alignment: .leading, spacing: 20) {
            // Header
            HStack(alignment: .top, spacing: 12) {
                Image(systemName: "arrow.down.circle.fill")
                    .font(.largeTitle)
                    .foregroundStyle(Color.accentColor)
                VStack(alignment: .leading, spacing: 4) {
                    Text("Boss \(update.version.description) is available")
                        .font(.title2.weight(.semibold))
                    if isDevBuild, let fv = fullVersion {
                        Label("Running a development build (\(fv))", systemImage: "hammer")
                            .font(.caption)
                            .foregroundStyle(.orange)
                    }
                }
            }

            // Release notes
            if !update.releaseNotes.isEmpty {
                VStack(alignment: .leading, spacing: 6) {
                    Text("Release Notes")
                        .font(.caption.weight(.semibold))
                        .foregroundStyle(.secondary)
                        .textCase(.uppercase)

                    ScrollView {
                        StructuredText(markdown: update.releaseNotes)
                            .bossMarkdown()
                            .frame(maxWidth: .infinity, alignment: .leading)
                            .padding(12)
                    }
                    .frame(maxHeight: 200)
                    .background(Color(nsColor: .controlBackgroundColor))
                    .clipShape(RoundedRectangle(cornerRadius: 8))
                    .overlay(
                        RoundedRectangle(cornerRadius: 8)
                            .stroke(Color(nsColor: .separatorColor), lineWidth: 0.5)
                    )
                }
            }

            Divider()

            // Buttons
            HStack(spacing: 8) {
                if !isDevBuild {
                    Button("Skip This Version") {
                        updateModel.skipCurrentVersion()
                        dismiss()
                    }
                    .foregroundStyle(.secondary)
                    .buttonStyle(.plain)
                }

                Spacer()

                Button("Later") { dismiss() }
                    .keyboardShortcut(.cancelAction)

                Button("Download") {
                    NSWorkspace.shared.open(update.assetURL)
                    dismiss()
                }
                .keyboardShortcut(.defaultAction)
            }
        }
    }

    // MARK: - Rate limited

    private func rateLimitedView(retryAfter: Date) -> some View {
        let formatted = retryAfter.formatted(.dateTime.hour().minute())
        return VStack(alignment: .leading, spacing: 20) {
            HStack(alignment: .top, spacing: 12) {
                Image(systemName: "clock.fill")
                    .font(.largeTitle)
                    .foregroundStyle(.orange)
                VStack(alignment: .leading, spacing: 4) {
                    Text("Rate limit reached")
                        .font(.title2.weight(.semibold))
                    Text("Too many requests to GitHub. Try again after \(formatted).")
                        .foregroundStyle(.secondary)
                }
            }
            Divider()
            HStack {
                Spacer()
                Button("OK") { dismiss() }
                    .keyboardShortcut(.defaultAction)
            }
        }
    }

    // MARK: - Network error

    private func errorView(message: String) -> some View {
        VStack(alignment: .leading, spacing: 20) {
            HStack(alignment: .top, spacing: 12) {
                Image(systemName: "exclamationmark.triangle.fill")
                    .font(.largeTitle)
                    .foregroundStyle(.orange)
                VStack(alignment: .leading, spacing: 4) {
                    Text("Update check failed")
                        .font(.title2.weight(.semibold))
                    Text(message)
                        .font(.callout)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
            Divider()
            HStack {
                Spacer()
                Button("OK") { dismiss() }
                    .keyboardShortcut(.defaultAction)
            }
        }
    }
}
