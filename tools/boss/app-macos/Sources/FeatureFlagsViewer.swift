import SwiftUI

/// Debug pane that lists every engine feature flag and lets the human
/// flip its value without rebuilding the engine. Backs incident 001
/// action item #5: the `detect_pr` cold-path fallback (the path that
/// mis-bound sibling PRs in the incident) is gated on
/// `detect_pr_cold_fallback`, so toggling that flag OFF here is the
/// kill-switch for the misbehaving path until the structural fix
/// (AI #6) lands.
///
/// The pane is read/write — the toggle sends a `set_feature_flag` RPC
/// and renders an in-flight indicator until the engine's
/// `feature_flag_set` echo lands. Optimistic UI patches mean the
/// toggle feels instantaneous; engine errors surface via the standard
/// `work_error` channel.
struct FeatureFlagsViewer: View {
    @EnvironmentObject private var chatModel: ChatViewModel

    var body: some View {
        VStack(spacing: 0) {
            header
            Divider()
            content
        }
        .onAppear {
            chatModel.refreshFeatureFlags()
        }
    }

    private var header: some View {
        HStack(spacing: 12) {
            Text("Feature Flags")
                .font(.headline)
            Spacer()
            Text("\(chatModel.featureFlags.count) flag\(chatModel.featureFlags.count == 1 ? "" : "s")")
                .font(.caption)
                .foregroundStyle(.secondary)
            Button(action: { chatModel.refreshFeatureFlags() }) {
                Image(systemName: "arrow.clockwise")
            }
            .buttonStyle(.borderless)
            .help("Re-read flags from the engine")
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 10)
    }

    @ViewBuilder
    private var content: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 18) {
                AppFeatureFlagsSection()
                if chatModel.featureFlags.isEmpty {
                    emptyEngineState
                } else {
                    ForEach(groupedFlags, id: \.0) { (category, flagsInCategory) in
                        FeatureFlagSection(
                            category: category,
                            flags: flagsInCategory
                        ) { name, enabled in
                            chatModel.setFeatureFlag(name: name, enabled: enabled)
                        }
                    }
                }
            }
            .padding(14)
        }
    }

    /// Flags grouped by category, preserving registry order within
    /// each group and category-first-seen order across groups.
    private var groupedFlags: [(String, [FeatureFlag])] {
        var seenCategoriesInOrder: [String] = []
        var byCategory: [String: [FeatureFlag]] = [:]
        for flag in chatModel.featureFlags {
            if byCategory[flag.category] == nil {
                seenCategoriesInOrder.append(flag.category)
            }
            byCategory[flag.category, default: []].append(flag)
        }
        return seenCategoriesInOrder.map { category in
            (category, byCategory[category] ?? [])
        }
    }

    private var emptyEngineState: some View {
        VStack(spacing: 10) {
            Text("No engine feature flags registered")
                .font(.headline)
            Text("The engine returned an empty flag set. This is unexpected — the registry should contain at least one entry. Try Refresh, or check the engine log.")
                .multilineTextAlignment(.center)
                .foregroundStyle(.secondary)
                .frame(maxWidth: 420)
        }
        .padding()
        .frame(maxWidth: .infinity)
    }
}

/// Section for app-local feature flags (stored in UserDefaults via @AppStorage,
/// not engine state). These take effect immediately without a restart.
private struct AppFeatureFlagsSection: View {
    @AppStorage("boss.ui.standardSearch") private var standardSearch: Bool = false

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            Text("APP (LOCAL)")
                .font(.caption.weight(.semibold))
                .foregroundStyle(.secondary)
                .tracking(0.5)
            VStack(spacing: 0) {
                appFlagRow(
                    name: "boss.ui.standardSearch",
                    description: "Use SwiftUI's platform-standard .searchable() for the work-board toolbar instead of the custom search item. Flip ON to validate the standard search path.",
                    defaultEnabled: false,
                    isOn: $standardSearch
                )
            }
            .background(
                RoundedRectangle(cornerRadius: 8, style: .continuous)
                    .fill(Color.gray.opacity(0.06))
            )
        }
    }

    @ViewBuilder
    private func appFlagRow(
        name: String,
        description: String,
        defaultEnabled: Bool,
        isOn: Binding<Bool>
    ) -> some View {
        HStack(alignment: .top, spacing: 14) {
            VStack(alignment: .leading, spacing: 4) {
                HStack(spacing: 6) {
                    Text(name)
                        .font(.system(.body, design: .monospaced).weight(.semibold))
                        .textSelection(.enabled)
                    if isOn.wrappedValue != defaultEnabled {
                        Text("override")
                            .font(.caption2.weight(.semibold))
                            .padding(.horizontal, 6)
                            .padding(.vertical, 2)
                            .background(
                                RoundedRectangle(cornerRadius: 4, style: .continuous)
                                    .fill(Color.accentColor.opacity(0.2))
                            )
                            .foregroundStyle(Color.accentColor)
                    }
                }
                Text(description)
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                Text("default: \(defaultEnabled ? "ON" : "OFF")")
                    .font(.caption2)
                    .foregroundStyle(.tertiary)
            }
            Spacer(minLength: 8)
            Toggle("", isOn: isOn)
                .labelsHidden()
                .toggleStyle(.switch)
                .padding(.top, 2)
        }
        .padding(12)
    }
}

private struct FeatureFlagSection: View {
    let category: String
    let flags: [FeatureFlag]
    let onToggle: (String, Bool) -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            Text(category.uppercased())
                .font(.caption.weight(.semibold))
                .foregroundStyle(.secondary)
                .tracking(0.5)
            VStack(spacing: 0) {
                ForEach(Array(flags.enumerated()), id: \.element.name) { idx, flag in
                    if idx > 0 {
                        Divider().padding(.leading, 12)
                    }
                    FeatureFlagRow(flag: flag) { enabled in
                        onToggle(flag.name, enabled)
                    }
                }
            }
            .background(
                RoundedRectangle(cornerRadius: 8, style: .continuous)
                    .fill(Color.gray.opacity(0.06))
            )
        }
    }
}

private struct FeatureFlagRow: View {
    let flag: FeatureFlag
    let onToggle: (Bool) -> Void

    var body: some View {
        HStack(alignment: .top, spacing: 14) {
            VStack(alignment: .leading, spacing: 4) {
                HStack(spacing: 6) {
                    Text(flag.name)
                        .font(.system(.body, design: .monospaced).weight(.semibold))
                        .textSelection(.enabled)
                    if flag.enabled != flag.defaultEnabled {
                        Text("override")
                            .font(.caption2.weight(.semibold))
                            .padding(.horizontal, 6)
                            .padding(.vertical, 2)
                            .background(
                                RoundedRectangle(cornerRadius: 4, style: .continuous)
                                    .fill(Color.accentColor.opacity(0.2))
                            )
                            .foregroundStyle(Color.accentColor)
                    }
                    if flag.enabled, flag.capabilityPresent == false {
                        Text("capability absent")
                            .font(.caption2.weight(.semibold))
                            .padding(.horizontal, 6)
                            .padding(.vertical, 2)
                            .background(
                                RoundedRectangle(cornerRadius: 4, style: .continuous)
                                    .fill(Color.orange.opacity(0.2))
                            )
                            .foregroundStyle(Color.orange)
                            .help("This flag is ON but its backing implementation is not compiled into this build — the flag will have no effect.")
                    }
                }
                Text(flag.description)
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                Text("default: \(flag.defaultEnabled ? "ON" : "OFF")")
                    .font(.caption2)
                    .foregroundStyle(.tertiary)
            }
            Spacer(minLength: 8)
            Toggle("", isOn: Binding(
                get: { flag.enabled },
                set: { newValue in onToggle(newValue) }
            ))
            .labelsHidden()
            .toggleStyle(.switch)
            .padding(.top, 2)
        }
        .padding(12)
    }
}
