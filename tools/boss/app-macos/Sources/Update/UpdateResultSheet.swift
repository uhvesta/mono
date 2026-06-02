import AppKit
import SwiftUI
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
            if !update.changelog.isEmpty || !update.releaseNotes.isEmpty {
                changelogView(changelog: update.changelog, fallbackNotes: update.releaseNotes)
            }

            // In-app download/stage status (release builds only).
            if !isDevBuild, let note = downloadStatusNote(for: update) {
                Text(note)
                    .font(.caption)
                    .foregroundStyle(downloadFailed(for: update) ? .orange : .secondary)
                    .fixedSize(horizontal: false, vertical: true)
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

                primaryActionButton(update: update, isDevBuild: isDevBuild)
            }
        }
    }

    // MARK: - Primary action (download / install)

    /// The trailing call-to-action. Dev builds keep the manual browser download (the
    /// updater never swaps over a dev build, per design non-goals). Release builds run
    /// the in-app pipeline: **Download** stages the verified bundle, then the button
    /// becomes **Install & Relaunch**, which swaps it in and relaunches.
    @ViewBuilder
    private func primaryActionButton(update: AvailableUpdate, isDevBuild: Bool) -> some View {
        if isDevBuild {
            Button("Download") {
                NSWorkspace.shared.open(update.assetURL)
                dismiss()
            }
            .keyboardShortcut(.defaultAction)
        } else {
            switch updateModel.downloadState {
            case .downloading(let v, _) where v == update.version:
                Button {
                } label: {
                    HStack(spacing: 6) {
                        ProgressView().controlSize(.small)
                        Text("Downloading…")
                    }
                }
                .disabled(true)

            case .readyToInstall(let v) where v == update.version:
                Button("Install & Relaunch") {
                    if UpdateLifecycle.installStagedAndRelaunch() {
                        // Swap applied + helper spawned; quit so it can relaunch us.
                        NSApplication.shared.terminate(nil)
                    } else {
                        // Not writable (/Applications without admin) or swap failed —
                        // fall back to the manual download so the user isn't stuck.
                        NSWorkspace.shared.open(update.assetURL)
                    }
                    dismiss()
                }
                .keyboardShortcut(.defaultAction)

            case .failed(let v, _) where v == update.version:
                Button("Retry Download") {
                    updateModel.downloadAvailableUpdate()
                }
                .keyboardShortcut(.defaultAction)

            default:
                Button("Download") {
                    updateModel.downloadAvailableUpdate()
                }
                .keyboardShortcut(.defaultAction)
            }
        }
    }

    /// One-line status under the release notes describing the current download/stage
    /// for `update`. `nil` when idle (the button text carries the affordance).
    private func downloadStatusNote(for update: AvailableUpdate) -> String? {
        switch updateModel.downloadState {
        case .downloading(let v, let fraction) where v == update.version:
            let pct = Int((fraction * 100).rounded())
            return pct > 0 ? "Downloading Boss \(update.version)… \(pct)%" : "Downloading Boss \(update.version)…"
        case .readyToInstall(let v) where v == update.version:
            return "Boss \(update.version) downloaded and verified. Install & Relaunch to apply it now."
        case .failed(let v, let reason) where v == update.version:
            return "Download failed: \(reason)"
        default:
            return nil
        }
    }

    private func downloadFailed(for update: AvailableUpdate) -> Bool {
        if case .failed(let v, _) = updateModel.downloadState, v == update.version { return true }
        return false
    }

    // MARK: - Changelog helpers

    @ViewBuilder
    private func changelogView(changelog: [ReleaseNote], fallbackNotes: String) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            Text("Release Notes")
                .font(.caption.weight(.semibold))
                .foregroundStyle(.secondary)
                .textCase(.uppercase)

            ScrollView {
                ReleaseNotesContent(changelog: changelog, fallbackNotes: fallbackNotes)
                    .padding(12)
            }
            .frame(maxHeight: 300)
            .background(Color(nsColor: .controlBackgroundColor))
            .clipShape(RoundedRectangle(cornerRadius: 8))
            .overlay(
                RoundedRectangle(cornerRadius: 8)
                    .stroke(Color(nsColor: .separatorColor), lineWidth: 0.5)
            )
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
