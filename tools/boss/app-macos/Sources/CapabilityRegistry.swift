/// Registry of capability IDs compiled into this build.
///
/// Feature-flag infrastructure uses this to detect when a flag is
/// enabled but its implementation was not included in the current
/// build — signalling a "flag has no effect" condition to the
/// operator via the Feature Flags debug pane.
///
/// ## Usage
///
/// Code that provides a feature capability calls ``register(_:)``
/// at startup. The app reports all registered IDs to the engine
/// via `RegisterCapabilities` after session establishment:
///
/// ```swift
/// // At startup, in the code that provides the capability:
/// CapabilityRegistry.shared.register("toolbar_search_standard")
/// ```
///
/// If the code path is conditionally compiled out of the build, the
/// registration never runs, so the capability is absent, and the
/// engine surfaces a warning when the operator enables the flag.
@MainActor
final class CapabilityRegistry {
    static let shared = CapabilityRegistry()

    private var ids: Set<String> = []

    private init() {}

    /// Register `id` as present in this build. Idempotent — calling
    /// it multiple times for the same id is safe.
    func register(_ id: String) {
        ids.insert(id)
    }

    /// All currently registered capability IDs. Sent to the engine
    /// via `RegisterCapabilities` at session startup so it can
    /// detect flag ↔ capability mismatches.
    var all: [String] {
        Array(ids)
    }
}
