import AppKit
import Foundation
import UpdateCore

/// App-lifecycle glue for the self-updater's install/swap step (design doc §4, T7).
///
/// All of the *mechanics* — the bundle rename, `.bak` rollback, first-launch-OK
/// flag, blocklist, reconciliation — live in `UpdateCore.UpdateInstaller`, which is
/// pure and unit-tested. This enum is the thin, untested-by-design seam that wires
/// those into the app: it reads the user's update mode, detects dev builds, locates
/// the running bundle and the bundled `relaunch-helper.sh`, and performs the two
/// lifecycle side-effects the installer deliberately avoids — spawning a detached
/// `Process` and exiting the app.
///
/// Boundary cases handled here so callers stay simple:
/// - **Mode gating.** Auto-swap only runs in `automatic` mode (read straight from
///   the `@AppStorage` key `boss.update.mode`, matching `UpdateModel`'s storage).
/// - **Dev builds.** A `-dev-` `BossFullVersion` never auto-installs (design §
///   non-goals); reconciliation/flagging still runs so a dev build can complete a
///   swap that a release build staged.
/// - **Non-bundle launches.** `swift run` / bazel-run launches have no `.app`
///   install location and no bundled helper; discovery returns nothing and every
///   path no-ops.
enum UpdateLifecycle {

    /// Matches `UpdateModel.StorageKeys.mode`.
    private static let modeKey = "boss.update.mode"
    /// Seconds the relaunch helper waits for the new version to report a clean launch
    /// before rolling back. Generous: a cold engine-coordinating launch is slow.
    private static let watchdogSeconds = 30

    // MARK: Environment probes

    static var isAutomaticMode: Bool {
        (UserDefaults.standard.string(forKey: modeKey) ?? "notify") == UpdateMode.automatic.rawValue
    }

    /// A build whose `BossFullVersion` contains `-dev-` is local/unreleased and must
    /// never be auto-swapped over (design doc non-goals + failure table).
    static var isDevBuild: Bool {
        let full = Bundle.main.object(forInfoDictionaryKey: "BossFullVersion") as? String
            ?? Bundle.main.infoDictionary?["CFBundleShortVersionString"] as? String ?? ""
        return full.contains("-dev-")
    }

    static var runningVersion: VersionTuple? {
        guard let short = Bundle.main.infoDictionary?["CFBundleShortVersionString"] as? String else { return nil }
        return VersionTuple.parse(short)
    }

    static func installer() -> UpdateInstaller {
        UpdateInstaller.live(installBundleURL: Bundle.main.bundleURL)
    }

    private static func helperScriptURL() -> URL? {
        Bundle.main.url(forResource: "relaunch-helper", withExtension: "sh")
    }

    // MARK: Lifecycle entry points

    /// Run once from `applicationDidFinishLaunching`. Completes a pending swap (or
    /// records a helper-driven rollback) and writes this version's first-launch-OK
    /// flag, which the relaunch watchdog polls for. Always safe; runs regardless of
    /// mode or build type because a swap staged by a release build must still settle
    /// even if the user has since switched to manual mode.
    static func reconcileAtLaunch() {
        guard let running = runningVersion else { return }
        let installer = installer()
        switch installer.reconcileAfterLaunch(currentVersion: running) {
        case .installed(let version):
            NSLog("[update] now running freshly-installed version \(version)")
        case .rolledBack(let version):
            NSLog("[update] update \(version) failed to launch and was rolled back + blocklisted")
        case .noChange:
            break
        }
        // Always (re)assert the flag last so a relaunch watchdog from the swap that
        // brought us here sees a healthy first launch even on the `.noChange` path.
        installer.markLaunchSucceeded(version: running)
    }

    /// Run at the startup chokepoint, *before* the engine launches (design §4
    /// "swap-on-startup fallback"). If a staged update can be swapped in, applies it
    /// in-process, spawns the detached relaunch helper, and returns `true` so the
    /// caller stops and lets the process exit — the helper waits for us to die, then
    /// relaunches into the new bundle. Returns `false` (and changes nothing) in every
    /// other case, so the caller proceeds with a normal launch.
    @discardableResult
    static func applyStartupSwapIfNeeded() -> Bool {
        applySwapIfNeeded(relaunch: true)
    }

    /// Run from `applicationWillTerminate` (design §4 "swap-on-quit"). Applies a
    /// staged update in place without relaunching; the next launch runs the new
    /// version. Best-effort and non-blocking — a failed swap leaves the current
    /// bundle untouched and the staged version waits for the next boundary.
    static func applyQuitSwapIfNeeded() {
        _ = applySwapIfNeeded(relaunch: false)
    }

    // MARK: - Implementation

    private static func applySwapIfNeeded(relaunch: Bool) -> Bool {
        guard isAutomaticMode, !isDevBuild, let running = runningVersion else { return false }
        let installer = installer()
        guard let ready = installer.newestReadyUpdate(currentVersion: running) else { return false }

        switch installer.planSwap(for: ready, currentVersion: running, relaunch: relaunch) {
        case .swap(let plan):
            do {
                try installer.applySwap(plan)
            } catch {
                NSLog("[update] swap failed (relaunch=\(relaunch)): \(error)")
                return false
            }

            guard relaunch else {
                NSLog("[update] swapped in \(plan.version) on quit; will run on next launch")
                return false
            }

            // Relaunch path: hand off to the detached helper, then exit.
            guard let script = helperScriptURL() else {
                // Swap already applied; without the helper we can't relaunch, but the
                // next manual launch will run the new version. Don't exit.
                NSLog("[update] swapped in \(plan.version) but relaunch-helper.sh is missing from the bundle")
                return false
            }
            let invocation = installer.helperInvocation(
                scriptURL: script, plan: plan, bossPID: getpid(), watchdogSeconds: watchdogSeconds)
            let proc = Process()
            proc.executableURL = invocation.executableURL
            proc.arguments = invocation.arguments
            do {
                try proc.run()
            } catch {
                NSLog("[update] failed to spawn relaunch helper: \(error)")
                return false
            }
            NSLog("[update] swapped in \(plan.version); relaunching via helper")
            return true

        case .notWritable(let installURL, let stagedURL):
            // /Applications-without-write: degrade gracefully (design §4). The UI
            // surfaces (T3/T4) reveal the staged bundle in Finder; here we only log.
            NSLog("[update] \(ready.version) ready but \(installURL.path) is not writable; staged at \(stagedURL.path)")
            return false

        case .blocked(let version):
            NSLog("[update] \(version) is blocklisted (failed a prior launch); not swapping")
            return false

        case .upToDate:
            return false
        }
    }
}
