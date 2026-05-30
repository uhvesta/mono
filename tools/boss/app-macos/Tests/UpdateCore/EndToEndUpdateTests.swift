import XCTest
@testable import UpdateCore

/// End-to-end integration tests for the complete detect → stage → swap → reconcile
/// pipeline (design doc §1–§4, T8 acceptance criterion).
///
/// These tests chain ``UpdateChecker``, ``UpdateDownloader``, and ``UpdateInstaller``
/// using real filesystem I/O and injected stubs for network and external tools.
/// They verify the component interactions — state-machine hand-offs, filesystem
/// conventions, failure recovery — that unit tests for each component in isolation
/// cannot cover.
///
/// Failure-injection scenarios covered (from the design doc failure table):
/// - Kill mid-download: a mid-download process kill leaves `.downloading` state;
///   `cleanup()` on the next startup sweeps the leftover and a fresh download succeeds.
/// - Corrupt staged zip: `ditto` extraction fails; no ready directory is promoted;
///   installer finds nothing; cleanup leaves a clean state.
/// - Non-launching build: rolled-back marker written by watchdog → blocklist → re-attempt blocked.
/// - ~/Applications (writable parent): full swap path proceeds end-to-end.
/// - /Applications (read-only parent): `.notWritable` graceful degradation.
final class EndToEndUpdateTests: XCTestCase {

    private var tempRoot: URL!

    override func setUpWithError() throws {
        tempRoot = FileManager.default.temporaryDirectory
            .appendingPathComponent("EndToEndUpdateTests-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: tempRoot, withIntermediateDirectories: true)
        try FileManager.default.createDirectory(at: updatesDir, withIntermediateDirectories: true)
        try FileManager.default.createDirectory(at: installParent, withIntermediateDirectories: true)
    }

    override func tearDownWithError() throws {
        if let tempRoot {
            try? FileManager.default.setAttributes(
                [.posixPermissions: 0o755], ofItemAtPath: installParent.path)
            try? FileManager.default.removeItem(at: tempRoot)
        }
    }

    // MARK: - Full pipeline: checker → downloader → installer → reconcile

    /// Chains all three components from detection through reconciliation:
    /// checker finds 1.0.28, downloader stages it to disk, installer discovers it
    /// from the filesystem (exercising the real manifest-based discovery path, not a
    /// direct reference hand-off), applies the swap, and reconciliation confirms a
    /// healthy first launch.
    func testFullPipelineDetectStageSwapReconcile() async throws {
        // 1. Checker detects 1.0.28 (stub HTTP, no network).
        let checker = makeChecker(current: "1.0.27", latestVersion: "1.0.28", assetSize: 64)
        let checkResult = await checker.checkForUpdates()
        guard case .available(let availableUpdate) = checkResult else {
            return XCTFail("checker must report .available; got \(checkResult)")
        }
        XCTAssertEqual(availableUpdate.version, v("1.0.28"))

        // 2. Downloader stages 1.0.28 (stub asset + tools, real filesystem).
        let downloader = makeDownloader(current: "1.0.27", assetBytes: availableUpdate.assetSize)
        let stageResult = await downloader.download(availableUpdate)
        guard case .ready = stageResult else {
            return XCTFail("downloader must return .ready; got \(stageResult)")
        }
        XCTAssertTrue(
            exists(updatesDir.appendingPathComponent("1.0.28/Boss.app")),
            "staged bundle must be present in Updates/1.0.28/Boss.app")

        // 3. Installer discovers the staged version from the filesystem manifest —
        //    the real discovery path that reads manifest.json and checks the bundle.
        plantInstalledBundle(marker: "old-1.0.27")
        let installer = makeInstaller()
        guard let ready = installer.newestReadyUpdate(currentVersion: v("1.0.27")) else {
            return XCTFail("installer must discover 1.0.28 as the newest ready update")
        }
        XCTAssertEqual(ready.version, v("1.0.28"))

        // 4. Plan and apply the swap.
        let plan = try unwrapSwap(
            installer.planSwap(for: ready, currentVersion: v("1.0.27"), relaunch: false))
        try installer.applySwap(plan)

        XCTAssertTrue(exists(installBundle), "new bundle must occupy the install location")
        XCTAssertEqual(markerAt(installer.backupURL), "old-1.0.27",
                       ".bak must hold the previous bundle's identity marker")
        XCTAssertNotNil(installer.pendingSwap(), "pending swap must be recorded in install-state.json")

        // 5. Reconcile on "next launch" (the new version is now running).
        let outcome = installer.reconcileAfterLaunch(currentVersion: v("1.0.28"))
        XCTAssertEqual(outcome, .installed(v("1.0.28")))
        XCTAssertTrue(installer.launchSucceeded(version: v("1.0.28")),
                      "first-launch-OK flag must be written after reconcile")
        XCTAssertNil(installer.pendingSwap(), "pending swap must be cleared after reconcile")
        XCTAssertFalse(exists(installer.backupURL),
                       ".bak must be dropped after a successful reconcile")
    }

    // MARK: - Failure injection: kill mid-download

    /// A process kill during a download leaves `.downloading` state in the staging
    /// work-directory (the downloader never gets a chance to write `.failed`). On the
    /// next startup, `cleanup()` sweeps all staging children regardless of manifest
    /// state, and a fresh download for the same version succeeds.
    func testKillMidDownloadStagingReclaimedOnRestart() async throws {
        // Simulate a killed process: manually plant staging/1.0.28/ in .downloading state.
        let stagingVersionDir = updatesDir.appendingPathComponent("staging/1.0.28")
        try FileManager.default.createDirectory(at: stagingVersionDir, withIntermediateDirectories: true)
        let killedManifest = UpdateManifest(
            version: "1.0.28", tag: "boss-v1.0.28",
            sourceURL: "https://example/Boss-1.0.28.zip",
            etag: nil, sha256: nil, verifiedAt: nil,
            state: .downloading, failureReason: nil)
        let data = try JSONEncoder().encode(killedManifest)
        try data.write(to: stagingVersionDir.appendingPathComponent("manifest.json"))

        XCTAssertTrue(exists(stagingVersionDir),
                      "staging leftover must be present before cleanup")
        XCTAssertFalse(exists(updatesDir.appendingPathComponent("1.0.28")),
                       "no promoted version dir must exist (kill happened before promotion)")

        // Restart: cleanup() sweeps ALL staging children, regardless of manifest state.
        let downloader = makeDownloader(current: "1.0.27", assetBytes: 64)
        await downloader.cleanup()

        let stagingContents = (try? FileManager.default.contentsOfDirectory(
            at: updatesDir.appendingPathComponent("staging"),
            includingPropertiesForKeys: nil)) ?? []
        XCTAssertTrue(stagingContents.isEmpty,
                      "staging must be empty after cleanup sweeps the kill leftover")

        // A fresh download for the same version must succeed after the cleanup.
        let update = makeAvailableUpdate(version: "1.0.28", assetSize: 64)
        let result = await downloader.download(update)
        guard case .ready(let staged) = result else {
            return XCTFail("fresh download after cleanup must succeed; got \(result)")
        }
        XCTAssertEqual(staged.version, v("1.0.28"))
        XCTAssertTrue(exists(updatesDir.appendingPathComponent("1.0.28/Boss.app")),
                      "staged bundle must be present after successful retry")
    }

    // MARK: - Failure injection: corrupt staged zip

    /// Simulates a corrupt downloaded archive where `ditto` extraction fails with a
    /// non-zero exit code. No version directory is promoted; the installer finds nothing
    /// ready; a subsequent cleanup reaps any staging leftover.
    func testCorruptStagedZipLeavesNoReadyVersion() async throws {
        let update = makeAvailableUpdate(version: "1.0.28", assetSize: 64)
        let corruptOps = BundleOperations(
            extract: { _, _ in
                throw BundleOperationError(
                    tool: "ditto", status: 1, message: "bad magic number in ZIP")
            },
            verifyCodeSignature: { _ in },
            readTeamID: { _ in nil },
            stripQuarantine: { _ in }
        )
        let downloader = UpdateDownloader(
            updatesDirectory: updatesDir,
            currentVersion: v("1.0.27"),
            runningTeamID: nil,
            assetDownloader: makeAssetDownloader(bytes: 64),
            bundleOps: corruptOps
        )

        let result = await downloader.download(update)
        guard case .failed(let reason) = result else {
            return XCTFail("corrupt archive must fail; got \(result)")
        }
        XCTAssertTrue(reason.contains("extraction failed"),
                      "failure reason must describe the extraction error; got: \(reason)")

        // No ready directory must be present.
        XCTAssertFalse(exists(updatesDir.appendingPathComponent("1.0.28")),
                       "no version dir must be promoted after extraction failure")

        // Installer must find nothing to swap.
        let installer = makeInstaller()
        XCTAssertNil(installer.newestReadyUpdate(currentVersion: v("1.0.27")),
                     "installer must find no ready update after extraction failure")

        // Cleanup sweeps any staging leftover.
        await downloader.cleanup()
        let stagingContents = (try? FileManager.default.contentsOfDirectory(
            at: updatesDir.appendingPathComponent("staging"),
            includingPropertiesForKeys: nil)) ?? []
        XCTAssertTrue(stagingContents.isEmpty, "staging must be empty after cleanup")
    }

    // MARK: - Failure injection: non-launching build → rollback

    /// Verifies the full rollback lifecycle: swap applied, new version fails to write its
    /// first-launch-OK flag (simulating a crash before `applicationDidFinishLaunching`),
    /// the relaunch-helper restores the backup and writes a rolled-back marker, then
    /// `reconcileAfterLaunch` reads the marker, blocklists the version, and prevents
    /// any subsequent swap attempt for that version.
    func testNonLaunchingBuildRollbackBlocklistsPermanently() throws {
        plantInstalledBundle(marker: "old-1.0.27")
        plantReadyVersion("1.0.28", marker: "new-1.0.28")
        let installer = makeInstaller()

        let ready = ReadyUpdate(
            version: v("1.0.28"), tag: "boss-v1.0.28",
            bundleURL: updatesDir.appendingPathComponent("1.0.28/Boss.app"))
        let plan = try unwrapSwap(
            installer.planSwap(for: ready, currentVersion: v("1.0.27"), relaunch: true))

        // Apply the swap: current bundle → .bak, staged bundle → install location.
        try installer.applySwap(plan)
        XCTAssertEqual(markerAt(installBundle), "new-1.0.28")
        XCTAssertEqual(markerAt(installer.backupURL), "old-1.0.27")

        // Simulate the relaunch-helper's watchdog timeout:
        //   new version never wrote the first-launch-OK flag;
        //   helper restores the backup bundle;
        //   helper writes the rolled-back marker.
        try FileManager.default.removeItem(at: installBundle)
        try FileManager.default.moveItem(at: installer.backupURL, to: installBundle)
        try "\(v("1.0.28").description)\n".write(
            to: installer.rolledBackMarkerURL, atomically: true, encoding: .utf8)
        XCTAssertFalse(installer.launchSucceeded(version: v("1.0.28")),
                       "first-launch-OK flag must not exist (new version crashed)")

        // Back on the old version after rollback. Reconcile processes the marker.
        let outcome = installer.reconcileAfterLaunch(currentVersion: v("1.0.27"))
        XCTAssertEqual(outcome, .rolledBack(v("1.0.28")))
        XCTAssertTrue(installer.isBlocked(v("1.0.28")),
                      "1.0.28 must be blocklisted after rollback")
        XCTAssertFalse(exists(installer.rolledBackMarkerURL),
                       "marker must be consumed by reconcile")
        XCTAssertNil(installer.pendingSwap(),
                     "pending swap must be cleared after rollback")

        // Subsequent planSwap for the blocklisted version must be rejected.
        // Re-plant the staged dir (reconcile reaped it).
        plantReadyVersion("1.0.28", marker: "new-1.0.28-retry")
        let ready2 = ReadyUpdate(
            version: v("1.0.28"), tag: "boss-v1.0.28",
            bundleURL: updatesDir.appendingPathComponent("1.0.28/Boss.app"))
        XCTAssertEqual(
            installer.planSwap(for: ready2, currentVersion: v("1.0.27"), relaunch: true),
            .blocked(v("1.0.28")),
            "planSwap must reject a blocklisted version")

        // newestReadyUpdate must also skip the blocklisted version.
        XCTAssertNil(installer.newestReadyUpdate(currentVersion: v("1.0.27")),
                     "installer discovery must skip blocklisted versions")
    }

    // MARK: - Install location: ~/Applications (user-writable)

    /// Verifies that the full swap path is available when the install parent is user-writable
    /// (the `~/Applications` default from the `.pkg` installer, requiring no admin password).
    func testUserApplicationsInstallAllowsFullSwap() throws {
        plantInstalledBundle(marker: "old-1.0.27")
        plantReadyVersion("1.0.28", marker: "new-1.0.28")
        let installer = makeInstaller()

        XCTAssertTrue(installer.isInstallLocationWritable(),
                      "install parent must be writable in the ~/Applications case")

        let ready = ReadyUpdate(
            version: v("1.0.28"), tag: "boss-v1.0.28",
            bundleURL: updatesDir.appendingPathComponent("1.0.28/Boss.app"))
        let decision = installer.planSwap(for: ready, currentVersion: v("1.0.27"), relaunch: false)
        guard case .swap(let plan) = decision else {
            return XCTFail("writable install parent must produce a .swap plan; got \(decision)")
        }
        try installer.applySwap(plan)
        XCTAssertEqual(markerAt(installBundle), "new-1.0.28",
                       "new bundle must be at the install location after swap")
        XCTAssertEqual(markerAt(installer.backupURL), "old-1.0.27",
                       ".bak must hold the previous bundle")
    }

    // MARK: - Install location: /Applications (admin required)

    /// Verifies graceful degradation when the install parent is not user-writable, as in
    /// a `/Applications` install. `planSwap` returns `.notWritable` so the app layer can
    /// surface a "reveal in Finder" fallback. No filesystem mutations occur.
    func testSystemApplicationsReadOnlyDegradesProperly() throws {
        plantInstalledBundle(marker: "old-1.0.27")
        plantReadyVersion("1.0.28", marker: "new-1.0.28")

        try FileManager.default.setAttributes(
            [.posixPermissions: 0o500], ofItemAtPath: installParent.path)
        defer {
            try? FileManager.default.setAttributes(
                [.posixPermissions: 0o755], ofItemAtPath: installParent.path)
        }

        let installer = makeInstaller()
        XCTAssertFalse(installer.isInstallLocationWritable(),
                       "install parent must not be writable in the /Applications case")

        let ready = ReadyUpdate(
            version: v("1.0.28"), tag: "boss-v1.0.28",
            bundleURL: updatesDir.appendingPathComponent("1.0.28/Boss.app"))
        let decision = installer.planSwap(for: ready, currentVersion: v("1.0.27"), relaunch: false)

        guard case .notWritable(let installURL, let stagedURL) = decision else {
            return XCTFail("non-writable install parent must produce .notWritable; got \(decision)")
        }
        XCTAssertEqual(installURL, installBundle)
        XCTAssertEqual(stagedURL, ready.bundleURL)
        XCTAssertEqual(markerAt(installBundle), "old-1.0.27",
                       "install location must remain unchanged when parent is not writable")
    }
}

// MARK: - Test fixtures

extension EndToEndUpdateTests {

    // MARK: Paths

    private var updatesDir: URL { tempRoot.appendingPathComponent("Updates", isDirectory: true) }
    private var installParent: URL { tempRoot.appendingPathComponent("Applications", isDirectory: true) }
    private var installBundle: URL { installParent.appendingPathComponent("Boss.app", isDirectory: true) }

    // MARK: Helpers

    private func v(_ s: String) -> VersionTuple { VersionTuple.parse(s)! }

    private func exists(_ url: URL) -> Bool {
        FileManager.default.fileExists(atPath: url.path)
    }

    private func makeInstaller() -> UpdateInstaller {
        UpdateInstaller(updatesDirectory: updatesDir, installBundleURL: installBundle)
    }

    // MARK: Checker stub

    /// Returns an ``UpdateChecker`` backed by a stub that serves a single `boss-v<latestVersion>`
    /// release with the given `assetSize`. No network is used.
    private func makeChecker(
        current: String, latestVersion: String, assetSize: Int
    ) -> UpdateChecker {
        let fetcher = HTTPFetcher { _ in
            let asset: [String: Any] = [
                "name": "Boss-\(latestVersion).zip",
                "size": assetSize,
                "browser_download_url":
                    "https://github.com/spinyfin/mono/releases/download/boss-v\(latestVersion)/Boss-\(latestVersion).zip",
            ]
            let release: [String: Any] = [
                "tag_name": "boss-v\(latestVersion)",
                "draft": false,
                "prerelease": false,
                "body": "Release notes for \(latestVersion)",
                "assets": [asset],
            ]
            let data = try JSONSerialization.data(withJSONObject: [release])
            let response = HTTPURLResponse(
                url: UpdateChecker.releasesURL, statusCode: 200,
                httpVersion: nil, headerFields: nil)!
            return (data, response)
        }
        return UpdateChecker(
            currentVersionString: current,
            fullVersionString: current,
            fetcher: fetcher
        )
    }

    // MARK: Downloader stubs

    private func makeAvailableUpdate(version: String, assetSize: Int) -> AvailableUpdate {
        AvailableUpdate(
            tagName: "boss-v\(version)",
            version: VersionTuple.parse(version)!,
            assetURL: URL(string:
                "https://github.com/spinyfin/mono/releases/download/boss-v\(version)/Boss-\(version).zip")!,
            assetSize: assetSize,
            releaseNotes: "Release notes for \(version)"
        )
    }

    private func makeAssetDownloader(bytes: Int) -> AssetDownloader {
        let root = tempRoot!
        return AssetDownloader { _, onProgress in
            let file = root.appendingPathComponent("dl-\(UUID().uuidString).zip")
            _ = FileManager.default.createFile(atPath: file.path, contents: Data(count: bytes))
            onProgress(1.0)
            return file
        }
    }

    private func passthroughOps(teamID: String?) -> BundleOperations {
        BundleOperations(
            extract: { _, dest in
                let bundle = dest.appendingPathComponent("Boss.app")
                try FileManager.default.createDirectory(
                    at: bundle.appendingPathComponent("Contents"), withIntermediateDirectories: true)
                _ = FileManager.default.createFile(
                    atPath: bundle.appendingPathComponent("Contents/Info.plist").path,
                    contents: Data("plist".utf8))
            },
            verifyCodeSignature: { _ in },
            readTeamID: { _ in teamID },
            stripQuarantine: { _ in }
        )
    }

    private func makeDownloader(current: String, assetBytes: Int) -> UpdateDownloader {
        UpdateDownloader(
            updatesDirectory: updatesDir,
            currentVersion: v(current),
            runningTeamID: nil,
            assetDownloader: makeAssetDownloader(bytes: assetBytes),
            bundleOps: passthroughOps(teamID: nil)
        )
    }

    // MARK: Installer fixtures

    private func makeBundle(at url: URL, marker: String) {
        let contents = url.appendingPathComponent("Contents")
        try? FileManager.default.createDirectory(at: contents, withIntermediateDirectories: true)
        try? marker.data(using: .utf8)!.write(to: contents.appendingPathComponent("which.txt"))
    }

    private func markerAt(_ bundle: URL) -> String? {
        guard let data = try? Data(
            contentsOf: bundle.appendingPathComponent("Contents/which.txt")) else { return nil }
        return String(data: data, encoding: .utf8)
    }

    private func plantInstalledBundle(marker: String) {
        makeBundle(at: installBundle, marker: marker)
    }

    private func plantReadyVersion(_ version: String, marker: String) {
        let dir = updatesDir.appendingPathComponent(version)
        makeBundle(at: dir.appendingPathComponent("Boss.app"), marker: marker)
        let manifest = UpdateManifest(
            version: version, tag: "boss-v\(version)",
            sourceURL: "https://example/\(version).zip",
            etag: nil, sha256: nil, verifiedAt: "2026-01-01T00:00:00Z",
            state: .ready, failureReason: nil)
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        guard let data = try? JSONEncoder().encode(manifest) else { return }
        try? data.write(to: dir.appendingPathComponent("manifest.json"))
    }

    private struct UnexpectedDecision: Error { let decision: SwapDecision }

    private func unwrapSwap(
        _ decision: SwapDecision,
        file: StaticString = #filePath,
        line: UInt = #line
    ) throws -> SwapPlan {
        guard case .swap(let plan) = decision else {
            XCTFail("expected .swap but got \(decision)", file: file, line: line)
            throw UnexpectedDecision(decision: decision)
        }
        return plan
    }
}
