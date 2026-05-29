import Foundation

// MARK: - Public install types

/// A verified, quarantine-stripped bundle discovered under `Updates/<version>/`,
/// ready for `UpdateInstaller` to swap into the live install location. Mirrors the
/// `internal` `StagedUpdate` that `UpdateDownloader` produces, but is `public` so the
/// app target can drive the swap without reaching into module-internal types.
public struct ReadyUpdate: Equatable, Sendable {
    public let version: VersionTuple
    public let tag: String
    /// `Updates/<version>/Boss.app`.
    public let bundleURL: URL

    public init(version: VersionTuple, tag: String, bundleURL: URL) {
        self.version = version
        self.tag = tag
        self.bundleURL = bundleURL
    }
}

/// The concrete file moves a swap performs, plus whether to relaunch afterwards.
///
/// `applySwap` executes this plan as two atomic-when-same-volume renames:
/// `installBundleURL → backupURL` then `stagedBundleURL → installBundleURL`. The
/// `~/Applications` common case keeps `Updates/` and the install location on the same
/// APFS volume, so both renames are atomic; `FileManager.moveItem` degrades to
/// copy+remove on the rare cross-volume case without breaking the rollback ordering.
public struct SwapPlan: Equatable, Sendable {
    public let version: VersionTuple
    /// Source: `Updates/<version>/Boss.app`.
    public let stagedBundleURL: URL
    /// Destination: the live bundle (`Bundle.main.bundleURL`).
    public let installBundleURL: URL
    /// Where the current bundle is moved aside, retained until first-launch-OK.
    public let backupURL: URL
    /// `true` for the startup / "Install & Relaunch" path (helper relaunches us);
    /// `false` for the plain-quit path (swap in place, next manual launch runs it).
    public let relaunch: Bool

    public init(
        version: VersionTuple,
        stagedBundleURL: URL,
        installBundleURL: URL,
        backupURL: URL,
        relaunch: Bool
    ) {
        self.version = version
        self.stagedBundleURL = stagedBundleURL
        self.installBundleURL = installBundleURL
        self.backupURL = backupURL
        self.relaunch = relaunch
    }
}

/// Why a swap can — or can't, or needn't — proceed. Drives graceful degradation:
/// the app surfaces `.notWritable` as "reveal the download in Finder" rather than
/// silently escalating privileges (design doc §4 privilege handling).
public enum SwapDecision: Equatable, Sendable {
    case swap(SwapPlan)
    /// The install location's parent directory isn't user-writable (e.g. a
    /// `/Applications` install without admin rights). No auto-swap in v1.
    case notWritable(installBundleURL: URL, stagedBundleURL: URL)
    /// This version previously failed its first launch and is blocklisted.
    case blocked(VersionTuple)
    /// Nothing newer than the running version is staged.
    case upToDate
}

/// Outcome of the at-launch reconciliation pass (`reconcileAfterLaunch`).
public enum LaunchReconciliation: Equatable, Sendable {
    /// A pending swap completed: we are now running the new version. `.bak` dropped.
    case installed(VersionTuple)
    /// The helper rolled back a version that failed its first launch; it has been
    /// blocklisted so we never re-attempt it.
    case rolledBack(VersionTuple)
    case noChange
}

/// A detached command the app layer spawns to relaunch into a freshly-swapped
/// bundle and watchdog its first launch. Built here (tested); executed by the app
/// (`Process`), which is a lifecycle side-effect that doesn't belong in this leaf.
public struct HelperInvocation: Equatable, Sendable {
    public let executableURL: URL
    public let arguments: [String]

    public init(executableURL: URL, arguments: [String]) {
        self.executableURL = executableURL
        self.arguments = arguments
    }
}

/// Errors from the swap mechanics. `applySwap` rolls back before throwing, so a
/// thrown error always leaves the install location holding the *previous* bundle.
public enum InstallerError: Error, CustomStringConvertible, Equatable {
    case backupFailed(String)
    case swapFailed(String)
    case noBackup

    public var description: String {
        switch self {
        case .backupFailed(let m): return "could not move current bundle aside: \(m)"
        case .swapFailed(let m): return "could not move staged bundle into place: \(m)"
        case .noBackup: return "no backup bundle to roll back to"
        }
    }
}

// MARK: - Persisted install state

/// `Updates/install-state.json` — the durable record the installer owns. The
/// rolled-back marker is *not* here: it's a separate plain-text file written by the
/// shell helper (which can't merge JSON), then folded into `blocklist` by the next
/// `reconcileAfterLaunch`.
private struct InstallState: Codable, Equatable {
    var pending: PendingRecord?
    var blocklist: [String]

    static let empty = InstallState(pending: nil, blocklist: [])
}

private struct PendingRecord: Codable, Equatable {
    let version: String
    let installURL: String
    let backupURL: String
    let stagedURL: String
    let relaunch: Bool
}

// MARK: - UpdateInstaller

/// Swaps a staged Boss release into the live install location and manages the
/// safety net around it (design doc §4): `.bak` rollback, the first-launch-OK flag
/// the relaunch watchdog polls, the failed-version blocklist, and the
/// `/Applications`-not-writable degradation path.
///
/// This type is a pure, synchronous value: every method touches only the filesystem
/// under `updatesDirectory` (plus the install location) and persists state in
/// `install-state.json`. There is no network, no `Process`, and no app-lifecycle
/// coupling — those live in the app target's `UpdateLifecycle`, which calls into here.
///
/// **Where the rename runs.** The design doc sketches a shell helper that performs
/// the bundle rename after the app exits. Per this project's module-placement rule
/// (swap mechanics must be unit-testable in `UpdateCore`), the rename instead runs
/// in-process here via `applySwap` at a safe boundary — startup *before* the engine
/// launches, or quit *after* the user has confirmed termination. Renaming a running
/// `.app` directory is the documented-safe operation (open handles keep the old
/// inode; the whole-bundle atomic rename never touches the signature). The shell
/// `relaunch-helper.sh` is reduced to the one thing Swift can't do after the process
/// exits: wait for the old PID, relaunch the new bundle, watchdog its first launch,
/// and roll back the `.bak` on failure.
public struct UpdateInstaller: Sendable {

    private let updatesDirectory: URL
    private let installBundleURL: URL

    public init(updatesDirectory: URL, installBundleURL: URL) {
        self.updatesDirectory = updatesDirectory
        self.installBundleURL = installBundleURL
    }

    /// Live installer rooted at `~/Library/Application Support/Boss/Updates` (the
    /// same directory `UpdateDownloader` stages into).
    public static func live(installBundleURL: URL) -> UpdateInstaller {
        UpdateInstaller(
            updatesDirectory: UpdateDownloader.defaultUpdatesDirectory(),
            installBundleURL: installBundleURL
        )
    }

    // MARK: Discovery

    /// The newest `.ready` staged version strictly greater than `currentVersion`
    /// whose bundle is present and which is not blocklisted; `nil` if none qualifies.
    public func newestReadyUpdate(currentVersion: VersionTuple) -> ReadyUpdate? {
        let blocked = blockedVersions()
        guard let entries = try? FileManager.default.contentsOfDirectory(
            at: updatesDirectory,
            includingPropertiesForKeys: [.isDirectoryKey],
            options: [.skipsHiddenFiles]
        ) else { return nil }

        var best: ReadyUpdate?
        for entry in entries {
            let name = entry.lastPathComponent
            if name == "staging" || name == "backup" { continue }
            guard isDirectory(entry),
                  let version = VersionTuple.parse(name),
                  version > currentVersion,
                  !blocked.contains(version),
                  let manifest = readManifest(at: entry),
                  manifest.state == .ready else { continue }
            let bundleURL = entry.appendingPathComponent("Boss.app", isDirectory: true)
            guard isDirectory(bundleURL) else { continue }

            if best == nil || version > best!.version {
                best = ReadyUpdate(version: version, tag: manifest.tag, bundleURL: bundleURL)
            }
        }
        return best
    }

    // MARK: Feasibility

    /// `true` when the install location's *parent* directory is writable, i.e. the
    /// bundle can be replaced without admin rights. This is the `~/Applications`
    /// common case; a `/Applications` install typically returns `false`.
    public func isInstallLocationWritable() -> Bool {
        let parent = installBundleURL.deletingLastPathComponent()
        return FileManager.default.isWritableFile(atPath: parent.path)
    }

    /// Default location for the set-aside previous bundle: `Updates/Boss.app.bak`.
    /// Sits on the same APFS volume as `~/Applications` in the supported case, so the
    /// `install → backup` rename stays atomic.
    public var backupURL: URL {
        updatesDirectory.appendingPathComponent("Boss.app.bak", isDirectory: true)
    }

    /// Decide whether `update` can be swapped in. Pure: performs no filesystem
    /// mutation. The caller (app layer) is responsible for mode/dev-build gating.
    public func planSwap(
        for update: ReadyUpdate,
        currentVersion: VersionTuple,
        relaunch: Bool
    ) -> SwapDecision {
        guard update.version > currentVersion else { return .upToDate }
        if isBlocked(update.version) { return .blocked(update.version) }
        guard isInstallLocationWritable() else {
            return .notWritable(installBundleURL: installBundleURL, stagedBundleURL: update.bundleURL)
        }
        return .swap(SwapPlan(
            version: update.version,
            stagedBundleURL: update.bundleURL,
            installBundleURL: installBundleURL,
            backupURL: backupURL,
            relaunch: relaunch
        ))
    }

    // MARK: Swap mechanics

    /// Execute `plan`: move the current bundle aside to `backupURL`, then move the
    /// staged bundle into the install location. Records a pending-swap entry first so
    /// the next launch can complete (or roll back) the transition.
    ///
    /// Rollback is built into the ordering: if the second move fails, the first is
    /// undone (backup restored over the install location) before throwing, so a
    /// thrown error always leaves the *previous* bundle live. The `.bak` is retained
    /// on success and dropped only once the new version reports a clean first launch.
    public func applySwap(_ plan: SwapPlan) throws {
        let fm = FileManager.default
        try? fm.createDirectory(
            at: plan.backupURL.deletingLastPathComponent(), withIntermediateDirectories: true)
        recordPending(plan)

        // 1. current → backup (skip if the install location is somehow absent).
        let hadInstall = fm.fileExists(atPath: plan.installBundleURL.path)
        if hadInstall {
            try? fm.removeItem(at: plan.backupURL)  // clear any stale backup
            do {
                try fm.moveItem(at: plan.installBundleURL, to: plan.backupURL)
            } catch {
                clearPending()
                throw InstallerError.backupFailed(error.localizedDescription)
            }
        }

        // 2. staged → install.
        do {
            try? fm.removeItem(at: plan.installBundleURL)
            try fm.moveItem(at: plan.stagedBundleURL, to: plan.installBundleURL)
        } catch {
            // Roll back: restore the previous bundle so we never end up with nothing.
            if hadInstall {
                try? fm.removeItem(at: plan.installBundleURL)
                try? fm.moveItem(at: plan.backupURL, to: plan.installBundleURL)
            }
            clearPending()
            throw InstallerError.swapFailed(error.localizedDescription)
        }
        // Success: the pending record + `.bak` survive until reconciliation.
    }

    /// Restore the backed-up bundle over the install location. Used for explicit
    /// recovery and exercised by tests; the production crash-watchdog rollback lives
    /// in `relaunch-helper.sh` because it must run after the process exits.
    public func rollback(_ plan: SwapPlan) throws {
        let fm = FileManager.default
        guard fm.fileExists(atPath: plan.backupURL.path) else { throw InstallerError.noBackup }
        try? fm.removeItem(at: plan.installBundleURL)
        try fm.moveItem(at: plan.backupURL, to: plan.installBundleURL)
    }

    // MARK: First-launch-OK flag

    /// The flag file the relaunch watchdog polls for. The freshly-launched new
    /// version writes this on `applicationDidFinishLaunching`; the helper clears it
    /// before relaunching, so its appearance unambiguously means "this launch reached
    /// a healthy state".
    public func firstLaunchFlagURL(for version: VersionTuple) -> URL {
        updatesDirectory.appendingPathComponent("launch-ok-\(version.description).flag")
    }

    /// Record that `version` launched successfully, and prune flags from other
    /// versions so the directory doesn't accumulate stale markers.
    public func markLaunchSucceeded(version: VersionTuple) {
        try? FileManager.default.createDirectory(at: updatesDirectory, withIntermediateDirectories: true)
        pruneLaunchFlags(except: version)
        try? Data().write(to: firstLaunchFlagURL(for: version))
    }

    public func launchSucceeded(version: VersionTuple) -> Bool {
        FileManager.default.fileExists(atPath: firstLaunchFlagURL(for: version).path)
    }

    // MARK: Blocklist

    public func isBlocked(_ version: VersionTuple) -> Bool {
        loadState().blocklist.contains(version.description)
    }

    public func blockedVersions() -> [VersionTuple] {
        loadState().blocklist.compactMap(VersionTuple.parse)
    }

    public func blockVersion(_ version: VersionTuple) {
        var state = loadState()
        if !state.blocklist.contains(version.description) {
            state.blocklist.append(version.description)
            saveState(state)
        }
    }

    // MARK: Pending swap

    /// The plan recorded by the most recent `applySwap`, if it hasn't been
    /// reconciled yet. `relaunch` is preserved but informational here.
    public func pendingSwap() -> SwapPlan? {
        guard let p = loadState().pending, let version = VersionTuple.parse(p.version) else { return nil }
        return SwapPlan(
            version: version,
            stagedBundleURL: URL(fileURLWithPath: p.stagedURL),
            installBundleURL: URL(fileURLWithPath: p.installURL),
            backupURL: URL(fileURLWithPath: p.backupURL),
            relaunch: p.relaunch
        )
    }

    // MARK: Reconciliation

    /// Run once at launch. Completes a pending swap or rolls back a failed one:
    ///
    /// 1. If the shell helper left a rolled-back marker, blocklist that version,
    ///    clear its staged directory, and report `.rolledBack`.
    /// 2. Otherwise, if a pending swap targeted the version we're now running, the
    ///    swap succeeded — drop the `.bak`, clear the pending record, report
    ///    `.installed`.
    /// 3. Otherwise `.noChange`.
    ///
    /// Order matters: the rolled-back marker is checked first because after a
    /// rollback we're running the *old* version, so the `pending.version == running`
    /// test in step 2 would (correctly) not fire — but we still must record the
    /// blocklist entry the marker represents.
    public func reconcileAfterLaunch(currentVersion: VersionTuple) -> LaunchReconciliation {
        let fm = FileManager.default

        if let rolledBack = readRolledBackMarker() {
            blockVersion(rolledBack)
            clearPending()
            try? fm.removeItem(at: stagedDirectory(for: rolledBack))
            try? fm.removeItem(at: backupURL)
            try? fm.removeItem(at: rolledBackMarkerURL)
            return .rolledBack(rolledBack)
        }

        if let pending = pendingSwap(), pending.version == currentVersion {
            markLaunchSucceeded(version: currentVersion)
            try? fm.removeItem(at: pending.backupURL)
            clearPending()
            return .installed(currentVersion)
        }

        return .noChange
    }

    // MARK: Helper invocation

    /// The path the shell helper writes the failed version into on a watchdog
    /// rollback; `reconcileAfterLaunch` reads and clears it on the next launch.
    public var rolledBackMarkerURL: URL {
        updatesDirectory.appendingPathComponent("rolled-back")
    }

    /// Build the detached `/bin/sh relaunch-helper.sh …` command for `plan`. The app
    /// layer spawns this *after* `applySwap` has already moved the new bundle into
    /// place, then exits; the helper waits for `bossPID`, relaunches, and watchdogs.
    public func helperInvocation(
        scriptURL: URL,
        plan: SwapPlan,
        bossPID: Int32,
        watchdogSeconds: Int
    ) -> HelperInvocation {
        HelperInvocation(
            executableURL: URL(fileURLWithPath: "/bin/sh"),
            arguments: [
                scriptURL.path,
                "--pid", String(bossPID),
                "--install", plan.installBundleURL.path,
                "--backup", plan.backupURL.path,
                "--flag", firstLaunchFlagURL(for: plan.version).path,
                "--rolled-back-marker", rolledBackMarkerURL.path,
                "--version", plan.version.description,
                "--watchdog", String(watchdogSeconds),
            ]
        )
    }

    // MARK: - Private helpers

    private var stateURL: URL { updatesDirectory.appendingPathComponent("install-state.json") }

    private func stagedDirectory(for version: VersionTuple) -> URL {
        updatesDirectory.appendingPathComponent(version.description, isDirectory: true)
    }

    private func loadState() -> InstallState {
        guard let data = try? Data(contentsOf: stateURL),
              let state = try? JSONDecoder().decode(InstallState.self, from: data) else {
            return .empty
        }
        return state
    }

    private func saveState(_ state: InstallState) {
        try? FileManager.default.createDirectory(at: updatesDirectory, withIntermediateDirectories: true)
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.prettyPrinted, .sortedKeys]
        guard let data = try? encoder.encode(state) else { return }
        try? data.write(to: stateURL, options: .atomic)
    }

    private func recordPending(_ plan: SwapPlan) {
        var state = loadState()
        state.pending = PendingRecord(
            version: plan.version.description,
            installURL: plan.installBundleURL.path,
            backupURL: plan.backupURL.path,
            stagedURL: plan.stagedBundleURL.path,
            relaunch: plan.relaunch
        )
        saveState(state)
    }

    private func clearPending() {
        var state = loadState()
        guard state.pending != nil else { return }
        state.pending = nil
        saveState(state)
    }

    private func readRolledBackMarker() -> VersionTuple? {
        guard let text = try? String(contentsOf: rolledBackMarkerURL, encoding: .utf8) else { return nil }
        return VersionTuple.parse(text.trimmingCharacters(in: .whitespacesAndNewlines))
    }

    private func pruneLaunchFlags(except keep: VersionTuple) {
        guard let entries = try? FileManager.default.contentsOfDirectory(
            at: updatesDirectory, includingPropertiesForKeys: nil, options: [.skipsHiddenFiles]
        ) else { return }
        let keepName = firstLaunchFlagURL(for: keep).lastPathComponent
        for entry in entries {
            let name = entry.lastPathComponent
            guard name.hasPrefix("launch-ok-"), name.hasSuffix(".flag"), name != keepName else { continue }
            try? FileManager.default.removeItem(at: entry)
        }
    }

    private func readManifest(at dir: URL) -> UpdateManifest? {
        let url = dir.appendingPathComponent("manifest.json")
        guard let data = try? Data(contentsOf: url) else { return nil }
        return try? JSONDecoder().decode(UpdateManifest.self, from: data)
    }

    private func isDirectory(_ url: URL) -> Bool {
        var isDir: ObjCBool = false
        return FileManager.default.fileExists(atPath: url.path, isDirectory: &isDir) && isDir.boolValue
    }
}
