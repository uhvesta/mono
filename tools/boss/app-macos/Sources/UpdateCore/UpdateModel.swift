import Foundation

// MARK: - UpdateMode

/// The three update-operation modes, ordered by escalating automation.
/// Stored in UserDefaults as a raw string so the value survives app upgrades.
public enum UpdateMode: String, CaseIterable, Sendable {
    /// No automatic polling; the user triggers checks manually via the menu item.
    case manual = "manual"
    /// Polls every 6 hours and shows a chrome badge when an update is available. Default.
    case notify = "notify"
    /// Same as notify, plus downloads and installs updates automatically at a safe boundary.
    case automatic = "automatic"
}

// MARK: - UpdateModel

/// Observable model that owns update state and drives the polling scheduler.
///
/// All state is `@MainActor`-isolated. App-layer SwiftUI views import this via `import UpdateCore`
/// and observe it directly — there is no back-dependency on app-level types.
///
/// **Lifecycle:** call `startPollingIfNeeded()` once from `applicationDidFinishLaunching`.
/// Manual checks are always available via `checkNow()` regardless of mode.
@MainActor
public final class UpdateModel: ObservableObject {

    // MARK: - Constants

    /// Default polling interval per the design (6 hours).
    public static let pollInterval: TimeInterval = 6 * 3600

    // MARK: - Published state

    @Published public private(set) var mode: UpdateMode
    @Published public private(set) var lastCheckDate: Date?
    @Published public private(set) var skippedVersion: String
    @Published public private(set) var lastCheckResult: UpdateCheckResult?
    @Published public private(set) var isChecking: Bool = false

    // MARK: - Private

    private enum StorageKeys {
        static let mode = "boss.update.mode"
        static let lastCheck = "boss.update.lastCheck"
        static let skippedVersion = "boss.update.skippedVersion"
    }

    private let defaults: UserDefaults
    private let checker: UpdateChecker
    private let jitterRange: ClosedRange<TimeInterval>
    private var schedulerTask: Task<Void, Never>?

    // MARK: - Init

    /// Convenience factory that reads version info from the running bundle.
    public static func fromBundle() -> UpdateModel? {
        guard let checker = UpdateChecker.fromBundle() else { return nil }
        return UpdateModel(checker: checker)
    }

    /// Designated initializer. `defaults` and `jitterRange` are injectable for testing.
    init(
        checker: UpdateChecker,
        defaults: UserDefaults = .standard,
        jitterRange: ClosedRange<TimeInterval> = 30...120
    ) {
        self.defaults = defaults
        self.checker = checker
        self.jitterRange = jitterRange

        let modeRaw = defaults.string(forKey: StorageKeys.mode) ?? UpdateMode.notify.rawValue
        self.mode = UpdateMode(rawValue: modeRaw) ?? .notify

        let epoch = defaults.double(forKey: StorageKeys.lastCheck)
        self.lastCheckDate = epoch == 0 ? nil : Date(timeIntervalSince1970: epoch)

        self.skippedVersion = defaults.string(forKey: StorageKeys.skippedVersion) ?? ""
    }

    // MARK: - Public API

    /// Starts the polling scheduler if the current mode enables it.
    /// Call once from `applicationDidFinishLaunching`.
    public func startPollingIfNeeded() {
        guard mode != .manual else { return }
        startPolling()
    }

    /// Changes the update mode, persists it, and starts or stops polling as needed.
    public func setMode(_ newMode: UpdateMode) {
        let oldMode = mode
        mode = newMode
        defaults.set(newMode.rawValue, forKey: StorageKeys.mode)
        switch (oldMode, newMode) {
        case (.manual, .notify), (.manual, .automatic):
            startPolling()
        case (_, .manual):
            stopPolling()
        default:
            break  // already polling; no restart needed for notify ↔ automatic transitions
        }
    }

    /// Performs an immediate update check regardless of mode, updates `lastCheckDate`,
    /// and returns the result. Safe to call from a "Check for Updates…" menu action.
    @discardableResult
    public func checkNow() async -> UpdateCheckResult {
        isChecking = true
        defer { isChecking = false }
        let result = await checker.checkForUpdates()
        lastCheckResult = result
        let now = Date()
        lastCheckDate = now
        defaults.set(now.timeIntervalSince1970, forKey: StorageKeys.lastCheck)
        return result
    }

    /// Records the current available version as skipped so the badge is suppressed in Notify mode.
    /// No-op if `lastCheckResult` is not `.available`.
    public func skipCurrentVersion() {
        guard case .available(let update) = lastCheckResult else { return }
        skippedVersion = update.version.description
        defaults.set(skippedVersion, forKey: StorageKeys.skippedVersion)
    }

    /// Clears any previously skipped version, re-enabling badge display if an update is available.
    public func clearSkippedVersion() {
        skippedVersion = ""
        defaults.removeObject(forKey: StorageKeys.skippedVersion)
    }

    // MARK: - Scheduler

    private func startPolling() {
        schedulerTask?.cancel()
        schedulerTask = Task { [weak self] in
            guard let self else { return }
            await self.runSchedulerLoop()
        }
    }

    private func stopPolling() {
        schedulerTask?.cancel()
        schedulerTask = nil
    }

    /// Runs the polling loop: jittered launch delay → check → interval sleep → repeat.
    /// Backs off to `retryAfter` on rate-limit responses. Exits cleanly on cancellation.
    private func runSchedulerLoop() async {
        // Jitter prevents a fleet of machines behind one NAT from hitting the API at the same second.
        let jitter = TimeInterval.random(in: jitterRange)
        do {
            try await Task.sleep(for: .seconds(jitter))
        } catch {
            return
        }

        while !Task.isCancelled {
            let result = await checkNow()

            let nextDelay: TimeInterval
            if case .rateLimited(let retryAfter) = result {
                // Suspend until the server clears our rate limit.
                nextDelay = max(retryAfter.timeIntervalSinceNow, 0)
            } else {
                nextDelay = Self.pollInterval
            }

            do {
                try await Task.sleep(for: .seconds(nextDelay))
            } catch {
                return
            }
        }
    }
}
