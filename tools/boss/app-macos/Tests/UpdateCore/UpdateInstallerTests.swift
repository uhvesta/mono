import XCTest
@testable import UpdateCore

/// Covers `UpdateInstaller` (design doc §4): ready-update discovery, the swap rename
/// mechanics with `.bak` rollback, the first-launch-OK flag, the failed-version
/// blocklist, post-launch reconciliation, `/Applications`-writability gating, and the
/// relaunch-helper invocation. Everything runs against a real per-test temporary
/// `Updates/` directory and a stand-in install location, so the filesystem moves and
/// JSON state persistence are exercised for real.
final class UpdateInstallerTests: XCTestCase {

    private var tempRoot: URL!

    override func setUpWithError() throws {
        tempRoot = FileManager.default.temporaryDirectory
            .appendingPathComponent("UpdateInstallerTests-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: tempRoot, withIntermediateDirectories: true)
        try FileManager.default.createDirectory(at: updatesDir, withIntermediateDirectories: true)
        try FileManager.default.createDirectory(at: installParent, withIntermediateDirectories: true)
    }

    override func tearDownWithError() throws {
        if let tempRoot {
            // Restore writability in case a test left a dir read-only.
            try? FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: installParent.path)
            try? FileManager.default.removeItem(at: tempRoot)
        }
    }

    // MARK: Discovery

    func testNewestReadyUpdateSelectsHighestReadyAboveCurrent() {
        plantReadyVersion("1.0.28")
        plantReadyVersion("1.0.30")
        plantReadyVersion("1.0.29")
        let installer = makeInstaller()

        let ready = installer.newestReadyUpdate(currentVersion: v("1.0.27"))
        XCTAssertEqual(ready?.version, v("1.0.30"))
        XCTAssertEqual(ready?.tag, "boss-v1.0.30")
        // `newestReadyUpdate` derives the bundle URL from `contentsOfDirectory(at:)`,
        // which on macOS returns entries rooted at the resolved `/private/var` form,
        // while `updatesDir` carries the `/var` symlink form. Canonicalize both with
        // `resolvingSymlinksInPath()` so the comparison reflects path identity, not the
        // incidental `/var` vs `/private/var` spelling.
        XCTAssertEqual(
            ready?.bundleURL.resolvingSymlinksInPath(),
            updatesDir.appendingPathComponent("1.0.30/Boss.app").resolvingSymlinksInPath())
    }

    func testNewestReadyUpdateIgnoresVersionsAtOrBelowCurrent() {
        plantReadyVersion("1.0.20")
        plantReadyVersion("1.0.27")
        let installer = makeInstaller()
        XCTAssertNil(installer.newestReadyUpdate(currentVersion: v("1.0.27")))
    }

    func testNewestReadyUpdateSkipsNonReadyManifests() {
        plantVersion("1.0.28", state: .verifying)
        let installer = makeInstaller()
        XCTAssertNil(installer.newestReadyUpdate(currentVersion: v("1.0.27")))
    }

    func testNewestReadyUpdateSkipsMissingBundle() {
        // Manifest is ready but the Boss.app directory is absent.
        let dir = updatesDir.appendingPathComponent("1.0.28")
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        writeManifest(version: "1.0.28", state: .ready, to: dir)
        let installer = makeInstaller()
        XCTAssertNil(installer.newestReadyUpdate(currentVersion: v("1.0.27")))
    }

    func testNewestReadyUpdateSkipsBlocklistedVersion() {
        plantReadyVersion("1.0.28")
        let installer = makeInstaller()
        installer.blockVersion(v("1.0.28"))
        XCTAssertNil(installer.newestReadyUpdate(currentVersion: v("1.0.27")))
    }

    func testNewestReadyUpdateIgnoresStagingAndBackupAndUnknownDirs() {
        plantReadyVersion("1.0.28")
        try? FileManager.default.createDirectory(
            at: updatesDir.appendingPathComponent("staging"), withIntermediateDirectories: true)
        try? FileManager.default.createDirectory(
            at: updatesDir.appendingPathComponent("backup"), withIntermediateDirectories: true)
        try? FileManager.default.createDirectory(
            at: updatesDir.appendingPathComponent("not-a-version"), withIntermediateDirectories: true)
        let installer = makeInstaller()
        XCTAssertEqual(installer.newestReadyUpdate(currentVersion: v("1.0.27"))?.version, v("1.0.28"))
    }

    // MARK: Feasibility

    func testInstallLocationWritableTrueForWritableParent() {
        XCTAssertTrue(makeInstaller().isInstallLocationWritable())
    }

    func testInstallLocationWritableFalseForReadOnlyParent() throws {
        try FileManager.default.setAttributes([.posixPermissions: 0o500], ofItemAtPath: installParent.path)
        XCTAssertFalse(makeInstaller().isInstallLocationWritable())
        try FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: installParent.path)
    }

    func testPlanSwapReturnsSwapWhenFeasible() {
        let ready = plantReadyAndDescribe("1.0.28")
        let decision = makeInstaller().planSwap(for: ready, currentVersion: v("1.0.27"), relaunch: true)
        guard case .swap(let plan) = decision else { return XCTFail("expected .swap, got \(decision)") }
        XCTAssertEqual(plan.version, v("1.0.28"))
        XCTAssertEqual(plan.installBundleURL, installBundle)
        XCTAssertEqual(plan.stagedBundleURL, ready.bundleURL)
        XCTAssertTrue(plan.relaunch)
    }

    func testPlanSwapUpToDateWhenNotNewer() {
        let ready = plantReadyAndDescribe("1.0.27")
        let decision = makeInstaller().planSwap(for: ready, currentVersion: v("1.0.27"), relaunch: true)
        XCTAssertEqual(decision, .upToDate)
    }

    func testPlanSwapBlockedWhenBlocklisted() {
        let ready = plantReadyAndDescribe("1.0.28")
        let installer = makeInstaller()
        installer.blockVersion(v("1.0.28"))
        XCTAssertEqual(
            installer.planSwap(for: ready, currentVersion: v("1.0.27"), relaunch: true),
            .blocked(v("1.0.28")))
    }

    func testPlanSwapNotWritableWhenParentReadOnly() throws {
        let ready = plantReadyAndDescribe("1.0.28")
        try FileManager.default.setAttributes([.posixPermissions: 0o500], ofItemAtPath: installParent.path)
        let decision = makeInstaller().planSwap(for: ready, currentVersion: v("1.0.27"), relaunch: true)
        try FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: installParent.path)
        guard case .notWritable(let installURL, let stagedURL) = decision else {
            return XCTFail("expected .notWritable, got \(decision)")
        }
        XCTAssertEqual(installURL, installBundle)
        XCTAssertEqual(stagedURL, ready.bundleURL)
    }

    // MARK: Swap mechanics

    func testApplySwapMovesStagedIntoPlaceAndBacksUpCurrent() throws {
        plantInstalledBundle(marker: "old")
        let ready = plantReadyAndDescribe("1.0.28", marker: "new-1.0.28")
        let installer = makeInstaller()
        let plan = try unwrapSwap(installer.planSwap(for: ready, currentVersion: v("1.0.27"), relaunch: true))

        try installer.applySwap(plan)

        XCTAssertEqual(marker(at: installBundle), "new-1.0.28", "install location holds the new bundle")
        XCTAssertEqual(marker(at: installer.backupURL), "old", ".bak holds the previous bundle")
        XCTAssertFalse(exists(ready.bundleURL), "staged bundle was moved, not copied")
        XCTAssertEqual(installer.pendingSwap()?.version, v("1.0.28"), "pending swap recorded")
    }

    func testApplySwapWithNoExistingInstallJustPlacesStaged() throws {
        // No install bundle present (fresh / unusual). Swap still places the staged one.
        let ready = plantReadyAndDescribe("1.0.28", marker: "new")
        let installer = makeInstaller()
        let plan = try unwrapSwap(installer.planSwap(for: ready, currentVersion: v("1.0.27"), relaunch: false))

        try installer.applySwap(plan)
        XCTAssertEqual(marker(at: installBundle), "new")
        XCTAssertFalse(exists(installer.backupURL), "no backup when there was nothing to back up")
    }

    func testApplySwapRollsBackWhenStagedMoveFails() throws {
        plantInstalledBundle(marker: "old")
        let installer = makeInstaller()
        // A plan whose staged bundle does not exist forces the second move to fail.
        let plan = SwapPlan(
            version: v("1.0.28"),
            stagedBundleURL: updatesDir.appendingPathComponent("1.0.28/Boss.app"),
            installBundleURL: installBundle,
            backupURL: installer.backupURL,
            relaunch: true)

        XCTAssertThrowsError(try installer.applySwap(plan)) { error in
            guard case InstallerError.swapFailed = error else {
                return XCTFail("expected .swapFailed, got \(error)")
            }
        }
        XCTAssertEqual(marker(at: installBundle), "old", "previous bundle restored after a failed swap")
        XCTAssertFalse(exists(installer.backupURL), "backup consumed by the rollback")
        XCTAssertNil(installer.pendingSwap(), "pending cleared after a failed swap")
    }

    func testRollbackRestoresBackupOverInstall() throws {
        plantInstalledBundle(marker: "old")
        let ready = plantReadyAndDescribe("1.0.28", marker: "new")
        let installer = makeInstaller()
        let plan = try unwrapSwap(installer.planSwap(for: ready, currentVersion: v("1.0.27"), relaunch: true))
        try installer.applySwap(plan)
        XCTAssertEqual(marker(at: installBundle), "new")

        try installer.rollback(plan)
        XCTAssertEqual(marker(at: installBundle), "old", "rollback restores the previous bundle")
        XCTAssertFalse(exists(installer.backupURL))
    }

    func testRollbackThrowsWhenNoBackup() {
        let installer = makeInstaller()
        let plan = SwapPlan(
            version: v("1.0.28"), stagedBundleURL: installBundle, installBundleURL: installBundle,
            backupURL: installer.backupURL, relaunch: true)
        XCTAssertThrowsError(try installer.rollback(plan)) { error in
            XCTAssertEqual(error as? InstallerError, .noBackup)
        }
    }

    // MARK: First-launch-OK flag

    func testMarkLaunchSucceededWritesFlagAndPrunesOthers() {
        let installer = makeInstaller()
        installer.markLaunchSucceeded(version: v("1.0.27"))
        XCTAssertTrue(installer.launchSucceeded(version: v("1.0.27")))

        installer.markLaunchSucceeded(version: v("1.0.28"))
        XCTAssertTrue(installer.launchSucceeded(version: v("1.0.28")))
        XCTAssertFalse(installer.launchSucceeded(version: v("1.0.27")), "stale flag pruned")
    }

    // MARK: Blocklist

    func testBlocklistPersistsAcrossInstances() {
        makeInstaller().blockVersion(v("1.0.28"))
        // A fresh installer over the same directory must see the persisted entry.
        let reopened = makeInstaller()
        XCTAssertTrue(reopened.isBlocked(v("1.0.28")))
        XCTAssertEqual(reopened.blockedVersions(), [v("1.0.28")])
    }

    func testBlockVersionIsIdempotent() {
        let installer = makeInstaller()
        installer.blockVersion(v("1.0.28"))
        installer.blockVersion(v("1.0.28"))
        XCTAssertEqual(installer.blockedVersions(), [v("1.0.28")])
    }

    // MARK: Reconciliation

    func testReconcileInstalledClearsPendingAndDropsBackup() throws {
        plantInstalledBundle(marker: "old")
        let ready = plantReadyAndDescribe("1.0.28", marker: "new")
        let installer = makeInstaller()
        let plan = try unwrapSwap(installer.planSwap(for: ready, currentVersion: v("1.0.27"), relaunch: true))
        try installer.applySwap(plan)
        XCTAssertTrue(exists(installer.backupURL))

        // We are now running the new version → reconciliation treats it as success.
        let outcome = installer.reconcileAfterLaunch(currentVersion: v("1.0.28"))
        XCTAssertEqual(outcome, .installed(v("1.0.28")))
        XCTAssertNil(installer.pendingSwap(), "pending cleared on success")
        XCTAssertFalse(exists(installer.backupURL), "backup dropped on success")
        XCTAssertTrue(installer.launchSucceeded(version: v("1.0.28")), "first-launch-OK flag written")
    }

    func testReconcileRolledBackBlocklistsAndCleansUp() throws {
        plantInstalledBundle(marker: "old")
        let ready = plantReadyAndDescribe("1.0.28", marker: "new")
        let installer = makeInstaller()
        let plan = try unwrapSwap(installer.planSwap(for: ready, currentVersion: v("1.0.27"), relaunch: true))
        try installer.applySwap(plan)

        // Simulate the helper's watchdog rollback: it restored the bundle and left a
        // marker naming the failed version. We're back on the old version.
        try "1.0.28\n".write(to: installer.rolledBackMarkerURL, atomically: true, encoding: .utf8)

        let outcome = installer.reconcileAfterLaunch(currentVersion: v("1.0.27"))
        XCTAssertEqual(outcome, .rolledBack(v("1.0.28")))
        XCTAssertTrue(installer.isBlocked(v("1.0.28")), "failed version blocklisted")
        XCTAssertNil(installer.pendingSwap(), "pending cleared")
        XCTAssertFalse(exists(updatesDir.appendingPathComponent("1.0.28")), "failed staged dir reaped")
        XCTAssertFalse(exists(installer.rolledBackMarkerURL), "marker consumed")
    }

    func testReconcileNoChangeWhenNothingPending() {
        let installer = makeInstaller()
        XCTAssertEqual(installer.reconcileAfterLaunch(currentVersion: v("1.0.27")), .noChange)
    }

    // MARK: Helper invocation

    func testHelperInvocationBuildsExpectedArguments() {
        let installer = makeInstaller()
        let plan = SwapPlan(
            version: v("1.0.28"),
            stagedBundleURL: updatesDir.appendingPathComponent("1.0.28/Boss.app"),
            installBundleURL: installBundle,
            backupURL: installer.backupURL,
            relaunch: true)
        let script = URL(fileURLWithPath: "/Apps/Boss.app/Contents/Resources/relaunch-helper.sh")

        let invocation = installer.helperInvocation(
            scriptURL: script, plan: plan, bossPID: 4242, watchdogSeconds: 30)

        XCTAssertEqual(invocation.executableURL, URL(fileURLWithPath: "/bin/sh"))
        XCTAssertEqual(invocation.arguments, [
            script.path,
            "--pid", "4242",
            "--install", installBundle.path,
            "--backup", installer.backupURL.path,
            "--flag", installer.firstLaunchFlagURL(for: v("1.0.28")).path,
            "--rolled-back-marker", installer.rolledBackMarkerURL.path,
            "--version", "1.0.28",
            "--watchdog", "30",
        ])
    }
}

// MARK: - Test fixtures

extension UpdateInstallerTests {

    private var updatesDir: URL { tempRoot.appendingPathComponent("Updates", isDirectory: true) }
    private var installParent: URL { tempRoot.appendingPathComponent("Applications", isDirectory: true) }
    private var installBundle: URL { installParent.appendingPathComponent("Boss.app", isDirectory: true) }

    private func v(_ s: String) -> VersionTuple { VersionTuple.parse(s)! }

    private func makeInstaller() -> UpdateInstaller {
        UpdateInstaller(updatesDirectory: updatesDir, installBundleURL: installBundle)
    }

    private func exists(_ url: URL) -> Bool { FileManager.default.fileExists(atPath: url.path) }

    private struct UnexpectedDecision: Error { let decision: SwapDecision }

    private func unwrapSwap(
        _ decision: SwapDecision, file: StaticString = #filePath, line: UInt = #line
    ) throws -> SwapPlan {
        guard case .swap(let plan) = decision else {
            XCTFail("expected .swap but got \(decision)", file: file, line: line)
            throw UnexpectedDecision(decision: decision)
        }
        return plan
    }

    /// Create a `.app` directory containing `Contents/which.txt = marker` so moves
    /// can be verified by identity.
    private func makeBundle(at url: URL, marker: String) {
        let contents = url.appendingPathComponent("Contents")
        try? FileManager.default.createDirectory(at: contents, withIntermediateDirectories: true)
        try? marker.data(using: .utf8)!.write(to: contents.appendingPathComponent("which.txt"))
    }

    private func marker(at bundle: URL) -> String? {
        guard let data = try? Data(contentsOf: bundle.appendingPathComponent("Contents/which.txt")) else {
            return nil
        }
        return String(data: data, encoding: .utf8)
    }

    private func plantInstalledBundle(marker: String) {
        makeBundle(at: installBundle, marker: marker)
    }

    private func writeManifest(version: String, state: UpdateManifest.State, to dir: URL) {
        let manifest = UpdateManifest(
            version: version, tag: "boss-v\(version)", sourceURL: "https://example/\(version).zip",
            etag: nil, sha256: nil, verifiedAt: "2026-01-01T00:00:00Z", state: state, failureReason: nil)
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        let data = try! JSONEncoder().encode(manifest)
        try? data.write(to: dir.appendingPathComponent("manifest.json"))
    }

    /// Plant a `.ready` staged version: `Updates/<v>/Boss.app` + manifest.
    private func plantReadyVersion(_ version: String, marker: String? = nil) {
        let dir = updatesDir.appendingPathComponent(version)
        makeBundle(at: dir.appendingPathComponent("Boss.app"), marker: marker ?? "staged-\(version)")
        writeManifest(version: version, state: .ready, to: dir)
    }

    /// Plant a staged version with an arbitrary manifest state.
    private func plantVersion(_ version: String, state: UpdateManifest.State) {
        let dir = updatesDir.appendingPathComponent(version)
        makeBundle(at: dir.appendingPathComponent("Boss.app"), marker: "staged-\(version)")
        writeManifest(version: version, state: state, to: dir)
    }

    /// Plant a ready version and return the `ReadyUpdate` describing it.
    private func plantReadyAndDescribe(_ version: String, marker: String? = nil) -> ReadyUpdate {
        plantReadyVersion(version, marker: marker)
        return ReadyUpdate(
            version: v(version), tag: "boss-v\(version)",
            bundleURL: updatesDir.appendingPathComponent("\(version)/Boss.app"))
    }
}
