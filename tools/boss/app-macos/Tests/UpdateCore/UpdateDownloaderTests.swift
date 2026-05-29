import XCTest
@testable import UpdateCore

// MARK: - UpdateDownloader tests

/// Covers the download → verify → stage → cleanup pipeline in `UpdateDownloader`
/// (design doc §3). All tests run against a real, per-test temporary `Updates/`
/// directory so the manifest state machine, atomic rename, and cleanup rules are
/// exercised against the actual filesystem; only the two external surfaces — the
/// network download and the `ditto`/`codesign`/`xattr` tool calls — are stubbed.
final class UpdateDownloaderTests: XCTestCase {

    private var tempRoot: URL!

    override func setUpWithError() throws {
        tempRoot = FileManager.default.temporaryDirectory
            .appendingPathComponent("UpdateDownloaderTests-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: tempRoot, withIntermediateDirectories: true)
    }

    override func tearDownWithError() throws {
        if let tempRoot { try? FileManager.default.removeItem(at: tempRoot) }
    }

    // MARK: Happy path

    func testReadyStagesBundleZipAndManifest() async throws {
        let downloader = makeDownloader(current: "1.0.27", runningTeamID: nil)
        let outcome = await downloader.download(update("1.0.28"))

        guard case .ready(let staged) = outcome else {
            return XCTFail("expected .ready, got \(outcome)")
        }
        XCTAssertEqual(staged.version, VersionTuple(major: 1, minor: 0, patch: 28))
        XCTAssertEqual(staged.tag, "boss-v1.0.28")

        let versionDir = updatesDir.appendingPathComponent("1.0.28")
        XCTAssertTrue(exists(versionDir.appendingPathComponent("Boss.app")), "bundle must be staged")
        XCTAssertTrue(exists(versionDir.appendingPathComponent("Boss-1.0.28.zip")), "zip must be kept")
        XCTAssertTrue(exists(versionDir.appendingPathComponent("manifest.json")), "manifest must be written")

        let manifest = try XCTUnwrap(readManifest(at: versionDir))
        XCTAssertEqual(manifest.state, .ready)
        XCTAssertEqual(manifest.version, "1.0.28")
        XCTAssertEqual(manifest.tag, "boss-v1.0.28")
        XCTAssertNotNil(manifest.verifiedAt, "verifiedAt set on ready")
        XCTAssertNotNil(manifest.sha256, "digest recorded")
        XCTAssertNil(manifest.failureReason)
    }

    func testReadyStripsQuarantineExactlyOnceAfterVerification() async throws {
        let log = CallLog()
        let ops = recordingOps(log: log, teamID: nil)
        let downloader = makeDownloader(current: "1.0.27", runningTeamID: nil, ops: ops)

        let outcome = await downloader.download(update("1.0.28"))
        guard case .ready = outcome else { return XCTFail("expected .ready, got \(outcome)") }

        // Quarantine strip must run exactly once, and only after extract + verify.
        XCTAssertEqual(log.events.filter { $0 == "stripQuarantine" }.count, 1)
        XCTAssertEqual(log.events, ["extract", "verifyCodeSignature", "readTeamID", "stripQuarantine"])
    }

    func testStagingDirectoryClearedAfterSuccess() async throws {
        let downloader = makeDownloader(current: "1.0.27", runningTeamID: nil)
        _ = await downloader.download(update("1.0.28"))
        let stagingChildren = (try? FileManager.default.contentsOfDirectory(
            atPath: updatesDir.appendingPathComponent("staging").path)) ?? []
        XCTAssertTrue(stagingChildren.isEmpty, "staging must be swept after a successful stage")
    }

    // MARK: Already-staged short-circuit

    func testAlreadyStagedSkipsDownload() async throws {
        let log = CallLog()
        // First download stages 1.0.28.
        let downloader = makeDownloader(
            current: "1.0.27", runningTeamID: nil,
            downloaderHook: { log.record("download") }
        )
        guard case .ready = await downloader.download(update("1.0.28")) else {
            return XCTFail("first download should be .ready")
        }
        XCTAssertEqual(log.events, ["download"])

        // Second download of the same version must not hit the network.
        let outcome = await downloader.download(update("1.0.28"))
        guard case .alreadyStaged(let staged) = outcome else {
            return XCTFail("expected .alreadyStaged, got \(outcome)")
        }
        XCTAssertEqual(staged.version, VersionTuple(major: 1, minor: 0, patch: 28))
        XCTAssertEqual(log.events, ["download"], "second call must not download again")
    }

    // MARK: Failure modes

    func testSizeMismatchFails() async throws {
        // Downloader writes 10 bytes but the asset claims 999.
        let downloader = makeDownloader(
            current: "1.0.27", runningTeamID: nil, downloadBytes: 10
        )
        let outcome = await downloader.download(update("1.0.28", assetSize: 999))
        guard case .failed(let reason) = outcome else {
            return XCTFail("expected .failed, got \(outcome)")
        }
        XCTAssertTrue(reason.contains("size mismatch"), reason)
        XCTAssertFalse(exists(updatesDir.appendingPathComponent("1.0.28")),
                       "no version dir on size-mismatch failure")
    }

    func testExtractionFailureFails() async throws {
        let ops = failingOps(extract: { _, _ in
            throw BundleOperationError(tool: "ditto", status: 1, message: "corrupt")
        })
        let downloader = makeDownloader(current: "1.0.27", runningTeamID: nil, ops: ops)

        let outcome = await downloader.download(update("1.0.28"))
        guard case .failed(let reason) = outcome else {
            return XCTFail("expected .failed, got \(outcome)")
        }
        XCTAssertTrue(reason.contains("extraction failed"), reason)
    }

    func testMissingBundleFails() async throws {
        // Extract succeeds but produces no Boss.app.
        let ops = failingOps(extract: { _, dest in
            try FileManager.default.createDirectory(at: dest, withIntermediateDirectories: true)
        })
        let downloader = makeDownloader(current: "1.0.27", runningTeamID: nil, ops: ops)

        let outcome = await downloader.download(update("1.0.28"))
        guard case .failed(let reason) = outcome else {
            return XCTFail("expected .failed, got \(outcome)")
        }
        XCTAssertTrue(reason.contains("no Boss.app"), reason)
    }

    func testCodeSignatureFailureFails() async throws {
        let ops = failingOps(verify: { _ in
            throw BundleOperationError(tool: "codesign", status: 1, message: "rejected")
        })
        let downloader = makeDownloader(current: "1.0.27", runningTeamID: nil, ops: ops)

        let outcome = await downloader.download(update("1.0.28"))
        guard case .failed(let reason) = outcome else {
            return XCTFail("expected .failed, got \(outcome)")
        }
        XCTAssertTrue(reason.contains("code signature"), reason)
        XCTAssertFalse(exists(updatesDir.appendingPathComponent("1.0.28")))
    }

    func testTeamIDMismatchFails() async throws {
        // Running bundle is Developer-ID (team ABC); staged bundle is ad-hoc (nil).
        let ops = passthroughOps(teamID: nil)
        let downloader = makeDownloader(current: "1.0.27", runningTeamID: "ABC123", ops: ops)

        let outcome = await downloader.download(update("1.0.28"))
        guard case .failed(let reason) = outcome else {
            return XCTFail("expected .failed, got \(outcome)")
        }
        XCTAssertTrue(reason.contains("Team ID mismatch"), reason)
    }

    func testMatchingTeamIDSucceeds() async throws {
        let ops = passthroughOps(teamID: "ABC123")
        let downloader = makeDownloader(current: "1.0.27", runningTeamID: "ABC123", ops: ops)

        let outcome = await downloader.download(update("1.0.28"))
        guard case .ready = outcome else { return XCTFail("expected .ready, got \(outcome)") }
    }

    func testBothNilTeamIDsMatchAdHocReality() async throws {
        // The current bazel-built releases are ad-hoc-signed: both Team IDs nil → match.
        let ops = passthroughOps(teamID: nil)
        let downloader = makeDownloader(current: "1.0.27", runningTeamID: nil, ops: ops)
        guard case .ready = await downloader.download(update("1.0.28")) else {
            return XCTFail("ad-hoc (nil == nil) Team IDs must match")
        }
    }

    func testDownloadErrorFails() async throws {
        let failing = AssetDownloader { _, _ in throw URLError(.timedOut) }
        let dl = UpdateDownloader(
            updatesDirectory: updatesDir, currentVersion: .init(major: 1, minor: 0, patch: 27),
            runningTeamID: nil, assetDownloader: failing, bundleOps: passthroughOps(teamID: nil)
        )
        let outcome = await dl.download(update("1.0.28"))
        guard case .failed(let reason) = outcome else {
            return XCTFail("expected .failed, got \(outcome)")
        }
        XCTAssertTrue(reason.contains("download failed"), reason)
    }

    func testFailureLeavesNoReadyVersionAndIsReapedByCleanup() async throws {
        let ops = failingOps(verify: { _ in
            throw BundleOperationError(tool: "codesign", status: 1, message: "nope")
        })
        let downloader = makeDownloader(current: "1.0.27", runningTeamID: nil, ops: ops)
        _ = await downloader.download(update("1.0.28"))

        // A subsequent cleanup() sweeps the failed staging leftover.
        await downloader.cleanup()
        let stagingChildren = (try? FileManager.default.contentsOfDirectory(
            atPath: updatesDir.appendingPathComponent("staging").path)) ?? []
        XCTAssertTrue(stagingChildren.isEmpty)
    }

    // MARK: Atomic rename over a stale same-version directory

    func testReplacesStaleSameVersionDirectory() async throws {
        // Plant a stale, non-ready 1.0.28 dir; a fresh successful download must replace it.
        let staleDir = updatesDir.appendingPathComponent("1.0.28")
        try FileManager.default.createDirectory(at: staleDir, withIntermediateDirectories: true)
        try "garbage".data(using: .utf8)!.write(to: staleDir.appendingPathComponent("leftover.txt"))
        writeManifestRaw(.init(version: "1.0.28", tag: "boss-v1.0.28", sourceURL: "x",
                               etag: nil, sha256: nil, verifiedAt: nil, state: .downloading,
                               failureReason: nil), to: staleDir)

        let downloader = makeDownloader(current: "1.0.27", runningTeamID: nil)
        guard case .ready = await downloader.download(update("1.0.28")) else {
            return XCTFail("expected .ready")
        }
        XCTAssertFalse(exists(staleDir.appendingPathComponent("leftover.txt")),
                       "stale contents must be gone after atomic replace")
        XCTAssertEqual(readManifest(at: staleDir)?.state, .ready)
    }

    // MARK: Cleanup rules

    func testCleanupKeepsNewestReadyAfterDownload() async throws {
        // Pre-stage an older ready version 1.0.27 (== current's successor candidate).
        plantReadyVersion("1.0.28")
        let downloader = makeDownloader(current: "1.0.26", runningTeamID: nil)

        guard case .ready = await downloader.download(update("1.0.29")) else {
            return XCTFail("expected .ready")
        }
        XCTAssertTrue(exists(updatesDir.appendingPathComponent("1.0.29")), "newest kept")
        XCTAssertFalse(exists(updatesDir.appendingPathComponent("1.0.28")), "older ready pruned")
    }

    func testCleanupDeletesVersionsAtOrBelowCurrent() async throws {
        plantReadyVersion("1.0.20")  // < current
        plantReadyVersion("1.0.27")  // == current
        plantReadyVersion("1.0.30")  // > current, newest → kept
        let downloader = makeDownloader(current: "1.0.27", runningTeamID: nil)

        let deleted = await downloader.cleanup()

        XCTAssertFalse(exists(updatesDir.appendingPathComponent("1.0.20")))
        XCTAssertFalse(exists(updatesDir.appendingPathComponent("1.0.27")))
        XCTAssertTrue(exists(updatesDir.appendingPathComponent("1.0.30")), "newest > current kept")
        XCTAssertTrue(deleted.contains(VersionTuple(major: 1, minor: 0, patch: 20)))
        XCTAssertTrue(deleted.contains(VersionTuple(major: 1, minor: 0, patch: 27)))
    }

    func testCleanupSweepsNonReadyLeftovers() async throws {
        plantReadyVersion("1.0.30")
        // A version dir whose manifest is not ready (interrupted run).
        let interrupted = updatesDir.appendingPathComponent("1.0.31")
        try FileManager.default.createDirectory(at: interrupted, withIntermediateDirectories: true)
        writeManifestRaw(.init(version: "1.0.31", tag: "boss-v1.0.31", sourceURL: "x",
                               etag: nil, sha256: nil, verifiedAt: nil, state: .verifying,
                               failureReason: nil), to: interrupted)
        let downloader = makeDownloader(current: "1.0.27", runningTeamID: nil)

        _ = await downloader.cleanup()

        XCTAssertFalse(exists(interrupted), "non-ready leftover must be swept even if version > current")
        XCTAssertTrue(exists(updatesDir.appendingPathComponent("1.0.30")), "ready version retained")
    }

    func testCleanupLeavesUnknownDirectoriesUntouched() async throws {
        let unknown = updatesDir.appendingPathComponent("not-a-version")
        try FileManager.default.createDirectory(at: unknown, withIntermediateDirectories: true)
        let downloader = makeDownloader(current: "1.0.27", runningTeamID: nil)

        _ = await downloader.cleanup()

        XCTAssertTrue(exists(unknown), "non-version directories must be left alone")
    }

    func testCleanupKeepsNewestAmongMultipleReady() async throws {
        plantReadyVersion("1.0.28")
        plantReadyVersion("1.0.29")
        plantReadyVersion("1.0.30")
        let downloader = makeDownloader(current: "1.0.27", runningTeamID: nil)

        _ = await downloader.cleanup()

        XCTAssertTrue(exists(updatesDir.appendingPathComponent("1.0.30")))
        XCTAssertFalse(exists(updatesDir.appendingPathComponent("1.0.29")))
        XCTAssertFalse(exists(updatesDir.appendingPathComponent("1.0.28")))
    }

    // MARK: Progress

    func testProgressCallbackInvoked() async throws {
        let log = CallLog()
        let downloader = makeDownloader(current: "1.0.27", runningTeamID: nil)
        _ = await downloader.download(update("1.0.28"), onProgress: { p in
            if p >= 1.0 { log.record("done") }
        })
        XCTAssertEqual(log.events, ["done"])
    }

    // MARK: Manifest Codable round-trip

    func testManifestRoundTrips() throws {
        let manifest = UpdateManifest(
            version: "1.0.28", tag: "boss-v1.0.28",
            sourceURL: "https://example.com/Boss-1.0.28.zip",
            etag: "\"abc\"", sha256: "deadbeef", verifiedAt: "2026-05-28T00:00:00Z",
            state: .ready, failureReason: nil
        )
        let data = try JSONEncoder().encode(manifest)
        let decoded = try JSONDecoder().decode(UpdateManifest.self, from: data)
        XCTAssertEqual(manifest, decoded)
    }

    func testParseTeamIdentifier() {
        XCTAssertEqual(
            ProcessTool.parseTeamIdentifier(from: "Executable=/x\nTeamIdentifier=ABC123\nSealed=yes"),
            "ABC123"
        )
        XCTAssertNil(ProcessTool.parseTeamIdentifier(from: "TeamIdentifier=not set"))
        XCTAssertNil(ProcessTool.parseTeamIdentifier(from: "Signature=adhoc\nInfo.plist=not bound"))
    }
}

// MARK: - Test fixtures

extension UpdateDownloaderTests {

    private var updatesDir: URL { tempRoot.appendingPathComponent("Updates", isDirectory: true) }

    private func exists(_ url: URL) -> Bool {
        FileManager.default.fileExists(atPath: url.path)
    }

    private func update(_ version: String, assetSize: Int = 64) -> AvailableUpdate {
        let v = VersionTuple.parse(version)!
        return AvailableUpdate(
            tagName: "boss-v\(version)",
            version: v,
            assetURL: URL(string: "https://github.com/spinyfin/mono/releases/download/boss-v\(version)/Boss-\(version).zip")!,
            assetSize: assetSize,
            releaseNotes: "Notes for \(version)"
        )
    }

    /// A stub `AssetDownloader` that writes `bytes` bytes to a temp file and
    /// optionally fires a hook (used to assert it ran / didn't run).
    private func stubDownloader(bytes: Int, hook: (@Sendable () -> Void)? = nil) -> AssetDownloader {
        let root = tempRoot!
        return AssetDownloader { _, onProgress in
            hook?()
            let file = root.appendingPathComponent("dl-\(UUID().uuidString).zip")
            _ = FileManager.default.createFile(atPath: file.path, contents: Data(count: bytes))
            onProgress(1.0)
            return file
        }
    }

    /// `BundleOperations` whose `extract` synthesises a minimal `Boss.app` so the
    /// downstream filesystem logic (locate / move / promote) runs for real.
    /// `passthroughOps` with one or two stages overridden (the struct's fields are
    /// `let`, so callers can't mutate a copy in place).
    private func failingOps(
        teamID: String? = nil,
        extract: (@Sendable (URL, URL) async throws -> Void)? = nil,
        verify: (@Sendable (URL) async throws -> Void)? = nil
    ) -> BundleOperations {
        let base = passthroughOps(teamID: teamID)
        return BundleOperations(
            extract: extract ?? base.extract,
            verifyCodeSignature: verify ?? base.verifyCodeSignature,
            readTeamID: base.readTeamID,
            stripQuarantine: base.stripQuarantine
        )
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

    /// Like `passthroughOps` but records each operation into `log` for ordering
    /// assertions. `BundleOperations` fields are `let`, so we wrap the base
    /// closures into a fresh struct rather than mutating.
    private func recordingOps(log: CallLog, teamID: String?) -> BundleOperations {
        let base = passthroughOps(teamID: teamID)
        return BundleOperations(
            extract: { zip, dest in log.record("extract"); try await base.extract(zip, dest) },
            verifyCodeSignature: { b in log.record("verifyCodeSignature"); try await base.verifyCodeSignature(b) },
            readTeamID: { b in log.record("readTeamID"); return try await base.readTeamID(b) },
            stripQuarantine: { b in log.record("stripQuarantine"); try await base.stripQuarantine(b) }
        )
    }

    private func makeDownloader(
        current: String,
        runningTeamID: String?,
        downloadBytes: Int = 64,
        downloaderHook: (@Sendable () -> Void)? = nil,
        ops: BundleOperations? = nil
    ) -> UpdateDownloader {
        UpdateDownloader(
            updatesDirectory: updatesDir,
            currentVersion: VersionTuple.parse(current)!,
            runningTeamID: runningTeamID,
            assetDownloader: stubDownloader(bytes: downloadBytes, hook: downloaderHook),
            bundleOps: ops ?? passthroughOps(teamID: runningTeamID)
        )
    }

    /// Plant a fully-formed, `ready` version directory (bundle + manifest).
    private func plantReadyVersion(_ version: String) {
        let dir = updatesDir.appendingPathComponent(version)
        let bundle = dir.appendingPathComponent("Boss.app")
        try? FileManager.default.createDirectory(at: bundle, withIntermediateDirectories: true)
        writeManifestRaw(.init(version: version, tag: "boss-v\(version)", sourceURL: "x",
                               etag: nil, sha256: nil, verifiedAt: "2026-01-01T00:00:00Z",
                               state: .ready, failureReason: nil), to: dir)
    }

    private func readManifest(at dir: URL) -> UpdateManifest? {
        guard let data = try? Data(contentsOf: dir.appendingPathComponent("manifest.json")) else { return nil }
        return try? JSONDecoder().decode(UpdateManifest.self, from: data)
    }

    private func writeManifestRaw(_ manifest: UpdateManifest, to dir: URL) {
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        let data = try! JSONEncoder().encode(manifest)
        try? data.write(to: dir.appendingPathComponent("manifest.json"))
    }
}

// MARK: - Thread-safe event log (mirrors RequestRecorder in UpdateCheckerTests)

private final class CallLog: @unchecked Sendable {
    private let lock = NSLock()
    private var storage: [String] = []

    var events: [String] {
        lock.lock(); defer { lock.unlock() }
        return storage
    }

    func record(_ event: String) {
        lock.lock(); storage.append(event); lock.unlock()
    }
}
