import Foundation
import os.log

private let modelLog = Logger(subsystem: "dev.spinyfin.bossmacapp", category: "updater")

// MARK: - ManualUpdateFeedback

/// Transient feedback state surfaced only by a user-initiated "Check for Updates" action.
/// Background polling never sets this — only ``UpdateModel/presentUpdateSheet()`` does.
public enum ManualUpdateFeedback: Equatable, Sendable {
    case checking
    case upToDate
    case networkError(String)
    case rateLimited(Date)
}

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

// MARK: - UpdateDownloadState

/// Lifecycle of the in-app download/stage step, surfaced to the UI so the Settings
/// pane, badge popover, and result sheet can show progress and the
/// "downloaded — will install on quit/relaunch" state.
///
/// This is distinct from ``UpdateCheckResult`` (which only reports *availability*).
/// A check finding an update sets `lastCheckResult = .available`; staging the bundle
/// for that update drives this enum from `.downloading` to `.readyToInstall`.
public enum UpdateDownloadState: Equatable, Sendable {
    /// No download in flight and nothing staged this session.
    case idle
    /// Actively downloading/verifying `version`. `fraction` is best-effort `0...1`.
    case downloading(version: VersionTuple, fraction: Double)
    /// `version` is downloaded, verified, and staged under `Updates/<version>/`; the
    /// next safe boundary (quit/startup in automatic mode, or an explicit
    /// "Install & Relaunch") swaps it in.
    case readyToInstall(version: VersionTuple)
    /// The download or one of its integrity checks failed.
    case failed(version: VersionTuple, reason: String)
}

// MARK: - UpdateStager

/// Seam over ``UpdateDownloader`` so ``UpdateModel`` can drive the download/stage
/// step without a hard dependency on the network/filesystem at test time.
///
/// `.live` builds a real `UpdateDownloader` rooted at the default Updates directory
/// and resolves the running bundle's signing Team ID lazily — the downloader rejects
/// a staged bundle whose Team ID differs from the running one, so a swap can never
/// move us to a differently-signed bundle (equal `nil`s — today's ad-hoc-signed
/// reality — match). `.noop` never stages (placeholder/preview/non-bundle models);
/// tests inject their own closure.
struct UpdateStager: Sendable {
    /// Download, verify, and stage `update`, reporting fractional progress. Returns
    /// the staged version on success (freshly staged or already present), `nil` if
    /// any step failed.
    let stage: @Sendable (
        _ update: AvailableUpdate,
        _ onProgress: @Sendable @escaping (Double) -> Void
    ) async -> VersionTuple?

    /// Never stages. Used by placeholder/preview models and non-bundle launches.
    static let noop = UpdateStager { _, _ in nil }

    /// Live stager: a real `UpdateDownloader` rooted at
    /// `~/Library/Application Support/Boss/Updates`, with the running bundle's Team ID
    /// resolved once and cached.
    static func live(currentVersion: VersionTuple) -> UpdateStager {
        let teamID = RunningTeamIDCache()
        return UpdateStager { update, onProgress in
            let downloader = UpdateDownloader.live(
                currentVersion: currentVersion,
                runningTeamID: await teamID.resolve()
            )
            switch await downloader.download(update, onProgress: onProgress) {
            case .ready(let staged), .alreadyStaged(let staged):
                return staged.version
            case .failed:
                return nil
            }
        }
    }
}

/// Resolves the running bundle's signing Team ID once and caches it. Reading the
/// Team ID shells out to `codesign`, so we avoid repeating it on every download.
private actor RunningTeamIDCache {
    private var cached: String??

    func resolve() async -> String? {
        if let cached { return cached }
        let value = (try? await BundleOperations.live.readTeamID(Bundle.main.bundleURL)) ?? nil
        cached = .some(value)
        return value
    }
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
    /// Set to `true` to present the update-result sheet. Dismissed when the user closes the sheet.
    @Published public var showUpdateSheet: Bool = false
    /// Transient toast state set only by ``presentUpdateSheet()``. `nil` when no toast is visible.
    @Published public private(set) var manualCheckFeedback: ManualUpdateFeedback? = nil
    /// State of the in-app download/stage step. Drives the badge/sheet/Settings
    /// "downloading…" / "ready to install" affordances. See ``UpdateDownloadState``.
    @Published public private(set) var downloadState: UpdateDownloadState = .idle

    // MARK: - Private

    private var toastDismissTask: Task<Void, Never>?
    private var stagingTask: Task<Void, Never>?

    private enum StorageKeys {
        static let mode = "boss.update.mode"
        static let lastCheck = "boss.update.lastCheck"
        static let skippedVersion = "boss.update.skippedVersion"
    }

    private let defaults: UserDefaults
    private let checker: UpdateChecker
    private let stager: UpdateStager
    private let jitterRange: ClosedRange<TimeInterval>
    private var schedulerTask: Task<Void, Never>?

    // MARK: - Init

    /// Convenience factory that reads version info from the running bundle and wires
    /// the live downloader so automatic mode can stage updates.
    public static func fromBundle() -> UpdateModel? {
        guard let checker = UpdateChecker.fromBundle() else { return nil }
        let current = (Bundle.main.infoDictionary?["CFBundleShortVersionString"] as? String)
            .flatMap(VersionTuple.parse) ?? VersionTuple(major: 0, minor: 0, patch: 0)
        return UpdateModel(checker: checker, stager: .live(currentVersion: current))
    }

    /// Returns a no-op model used when the bundle is unavailable (e.g. SwiftUI previews).
    /// Badge is never shown; checks are no-ops.
    public static func placeholder() -> UpdateModel {
        UpdateModel(
            checker: UpdateChecker(
                currentVersionString: "0.0.0",
                fullVersionString: "0.0.0-dev-placeholder",
                fetcher: .noop
            )
        )
    }

    /// Designated initializer. `defaults`, `jitterRange`, and `stager` are injectable
    /// for testing. `stager` defaults to `.noop` so tests that don't exercise the
    /// download path never touch the network/filesystem.
    init(
        checker: UpdateChecker,
        defaults: UserDefaults = .standard,
        jitterRange: ClosedRange<TimeInterval> = 30...120,
        stager: UpdateStager = .noop
    ) {
        self.defaults = defaults
        self.checker = checker
        self.stager = stager
        self.jitterRange = jitterRange

        let modeRaw = defaults.string(forKey: StorageKeys.mode) ?? UpdateMode.notify.rawValue
        self.mode = UpdateMode(rawValue: modeRaw) ?? .notify

        let epoch = defaults.double(forKey: StorageKeys.lastCheck)
        self.lastCheckDate = epoch == 0 ? nil : Date(timeIntervalSince1970: epoch)

        self.skippedVersion = defaults.string(forKey: StorageKeys.skippedVersion) ?? ""
    }

    // MARK: - Public API

    /// `true` when the running binary is a dev build (BossFullVersion contains `-dev-`).
    /// The sheet uses this to suppress auto-install while still showing update availability.
    public var isDevBuild: Bool { checker.isDevBuild }

    /// Factory that always returns a model. Falls back to a version-less dev build if
    /// the bundle's `CFBundleShortVersionString` is unavailable (e.g. `swift run` without plist).
    public static func makeForApp() -> UpdateModel {
        if let model = fromBundle() { return model }
        // No `.app` bundle (e.g. `swift run` / bazel-run local dev). This is always a
        // dev build, which never auto-stages, so a `.noop` stager is correct.
        return UpdateModel(
            checker: UpdateChecker(
                currentVersionString: "0.0.0",
                fullVersionString: "0.0.0-dev-local",
                fetcher: .live
            ),
            stager: .noop
        )
    }

    /// Starts a manual update check and surfaces transient toast feedback.
    /// Shows the result sheet only when an update is available; all other outcomes
    /// (up-to-date, error, rate-limited) show a self-dismissing toast instead.
    public func presentUpdateSheet() {
        modelLog.info("update check triggered: source=manual-menu")
        toastDismissTask?.cancel()
        manualCheckFeedback = .checking
        Task {
            let result = await checkNow()
            switch result {
            case .upToDate:
                manualCheckFeedback = .upToDate
                scheduleFeedbackDismissal()
            case .available:
                manualCheckFeedback = nil
                showUpdateSheet = true
            case .networkError(let message):
                manualCheckFeedback = .networkError(message)
                scheduleFeedbackDismissal()
            case .rateLimited(let retryAfter):
                manualCheckFeedback = .rateLimited(retryAfter)
                scheduleFeedbackDismissal()
            }
        }
    }

    private func scheduleFeedbackDismissal() {
        toastDismissTask?.cancel()
        toastDismissTask = Task {
            try? await Task.sleep(for: .seconds(3))
            guard !Task.isCancelled else { return }
            manualCheckFeedback = nil
        }
    }

    /// Starts the polling scheduler if the current mode enables it.
    /// Call once from `applicationDidFinishLaunching`.
    public func startPollingIfNeeded() {
        guard mode != .manual else {
            modelLog.info("update polling: skipped (mode=manual)")
            return
        }
        modelLog.info("update polling: starting background scheduler (mode=\(self.mode.rawValue, privacy: .public) intervalSeconds=\(Self.pollInterval, privacy: .public))")
        startPolling()
    }

    /// Changes the update mode, persists it, and starts or stops polling as needed.
    public func setMode(_ newMode: UpdateMode) {
        let oldMode = mode
        modelLog.info("update mode changed: \(oldMode.rawValue, privacy: .public) → \(newMode.rawValue, privacy: .public)")
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

        switch result {
        case .upToDate:
            modelLog.info("update check complete: up-to-date")
        case .available(let update):
            modelLog.info(
                "update check complete: update available version=\(update.version.description, privacy: .public) tag=\(update.tagName, privacy: .public)"
            )
        case .rateLimited(let retryAfter):
            modelLog.warning("update check complete: rate-limited, retry after \(retryAfter.description, privacy: .public)")
        case .networkError(let message):
            modelLog.error("update check complete: network error — \(message, privacy: .public)")
        }

        // In automatic mode, a known-available update is downloaded and staged here
        // so the next quit/startup boundary can swap it in. Without this step the
        // installer's `newestReadyUpdate` never finds anything and the app stays on
        // the running version forever even with "Automatic" selected.
        maybeAutoStage(result)

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

    // MARK: - Download / stage

    /// User-initiated download of the currently-available update (the result sheet /
    /// badge "Download" button). Stages regardless of mode — the user explicitly
    /// asked for it. Dev builds never stage (auto-install is a non-goal for them);
    /// no-op when no update is available.
    public func downloadAvailableUpdate() {
        guard !isDevBuild, case .available(let update) = lastCheckResult else { return }
        beginStaging(update)
    }

    /// Automatic-mode auto-stage: begin downloading/staging an available update so a
    /// later quit/startup boundary can swap it in. No-op for non-automatic modes, dev
    /// builds, or non-`.available` results. Idempotent — see ``beginStaging(_:)``.
    private func maybeAutoStage(_ result: UpdateCheckResult) {
        guard mode == .automatic, !isDevBuild else { return }
        guard case .available(let update) = result else { return }
        beginStaging(update)
    }

    /// Kick off (or resume awareness of) a download for `update`. Idempotent: if we're
    /// already downloading or have already staged this exact version, it does nothing,
    /// so repeated checks (or a manual click on top of an auto-download) never restart
    /// an in-flight or completed stage. A newer version supersedes an older in-flight one.
    private func beginStaging(_ update: AvailableUpdate) {
        let version = update.version
        switch downloadState {
        case .downloading(let v, _) where v == version: return
        case .readyToInstall(let v) where v == version: return
        default: break
        }
        stagingTask?.cancel()
        downloadState = .downloading(version: version, fraction: 0)
        modelLog.info("update download: staging version=\(version.description, privacy: .public)")
        stagingTask = Task { [weak self] in
            guard let self else { return }
            let staged = await self.stager.stage(update) { [weak self] fraction in
                Task { @MainActor in
                    guard let self else { return }
                    // Only advance progress if we're still on this version's download.
                    if case .downloading(let v, _) = self.downloadState, v == version {
                        self.downloadState = .downloading(version: version, fraction: fraction)
                    }
                }
            }
            guard !Task.isCancelled else { return }
            if let staged {
                modelLog.info("update download: staged and ready version=\(staged.description, privacy: .public)")
                self.downloadState = .readyToInstall(version: staged)
            } else {
                modelLog.error("update download: staging failed version=\(version.description, privacy: .public)")
                self.downloadState = .failed(version: version, reason: "Download or verification failed")
            }
        }
    }

    /// Test hook: await the in-flight staging task, if any, so tests can wait for the
    /// background download to settle before asserting on ``downloadState``.
    func awaitStagingForTesting() async {
        await stagingTask?.value
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
        modelLog.info("update scheduler: first check in \(jitter, privacy: .public)s (jitter)")
        do {
            try await Task.sleep(for: .seconds(jitter))
        } catch {
            return
        }

        while !Task.isCancelled {
            modelLog.info("update check triggered: source=background-poll")
            let result = await checkNow()

            let nextDelay: TimeInterval
            if case .rateLimited(let retryAfter) = result {
                // Suspend until the server clears our rate limit.
                nextDelay = max(retryAfter.timeIntervalSinceNow, 0)
            } else {
                nextDelay = Self.pollInterval
            }

            modelLog.info("update scheduler: next poll in \(nextDelay, privacy: .public)s")
            do {
                try await Task.sleep(for: .seconds(nextDelay))
            } catch {
                return
            }
        }
    }
}
