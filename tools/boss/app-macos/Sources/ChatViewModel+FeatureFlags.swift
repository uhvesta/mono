import Foundation

extension ChatViewModel {
    // MARK: - Feature flags and metrics

    /// Ask the engine for a fresh snapshot of every registered metric.
    /// Called by the Metrics debug pane on appear and by its 5-second
    /// polling timer so values refresh without a manual reload.
    func refreshMetrics() {
        engine.sendMetricsListLive()
    }

    /// Ask the engine for the current feature-flag snapshot. Called by
    /// the Feature Flags debug pane on appear so the rendered state
    /// reflects whatever the engine has persisted (which may differ
    /// from what an earlier session in this app saw).
    func refreshFeatureFlags() {
        engine.sendListFeatureFlags()
    }

    /// Returns true when the `editorial_controls` engine flag is ON.
    /// Drives all editorial-controls UI gating: toolbar button, sheet, and
    /// any other entry points. Defaults to false until the flag snapshot
    /// arrives from the engine.
    var isEditorialControlsEnabled: Bool {
        featureFlags.first(where: { $0.name == "editorial_controls" })?.enabled ?? false
    }

    /// Toggle a feature flag. Optimistically patches the cached
    /// snapshot so the UI feels instantaneous; the engine's
    /// `feature_flag_set` echo reconciles state once the on-disk
    /// write returns. If the engine rejects the call (unknown flag,
    /// IO error), the echo never arrives and the `work_error` path
    /// surfaces the failure — the next `refreshFeatureFlags()` corrects
    /// the optimistic UI state.
    func setFeatureFlag(name: String, enabled: Bool) {
        if let idx = featureFlags.firstIndex(where: { $0.name == name }) {
            let prior = featureFlags[idx]
            featureFlags[idx] = FeatureFlag(
                name: prior.name,
                description: prior.description,
                category: prior.category,
                defaultEnabled: prior.defaultEnabled,
                enabled: enabled,
                capabilityPresent: prior.capabilityPresent
            )
        }
        engine.sendSetFeatureFlag(name: name, enabled: enabled)
    }
}
