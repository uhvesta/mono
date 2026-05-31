import XCTest
@testable import UpdateCore

// MARK: - UpdateChecker tests

/// Covers the release-detection logic in `UpdateChecker`:
/// - GitHub Releases endpoint fetching with correct headers
/// - ETag conditional-request round-trip
/// - `boss-v*` tag filtering, draft/prerelease exclusion, non-Boss tag exclusion
/// - Assetless-release skipping (handles `boss-v1.0.21`-style gaps)
/// - Maximum-tuple selection from an interleaved-order release list
/// - Semver-tuple comparison vs the running bundle version
/// - Rate-limit response handling (403 / 429)
/// - Dev-build flag detection
///
/// All tests use stub `HTTPFetcher` instances — no network, no real bundle reads.
final class UpdateCheckerTests: XCTestCase {

    // MARK: Version comparison

    func testVersionTupleOrdering() {
        XCTAssertLessThan(
            VersionTuple(major: 1, minor: 0, patch: 9),
            VersionTuple(major: 1, minor: 0, patch: 18)
        )
        XCTAssertLessThan(
            VersionTuple(major: 1, minor: 0, patch: 27),
            VersionTuple(major: 1, minor: 1, patch: 0)
        )
        XCTAssertLessThan(
            VersionTuple(major: 1, minor: 0, patch: 99),
            VersionTuple(major: 2, minor: 0, patch: 0)
        )
        XCTAssertEqual(
            VersionTuple(major: 1, minor: 0, patch: 5),
            VersionTuple(major: 1, minor: 0, patch: 5)
        )
    }

    func testVersionTupleParsing() {
        XCTAssertEqual(
            VersionTuple.parse("1.0.28"),
            VersionTuple(major: 1, minor: 0, patch: 28)
        )
        XCTAssertNil(VersionTuple.parse("1.0"))
        XCTAssertNil(VersionTuple.parse("1.0.x"))
        XCTAssertNil(VersionTuple.parse(""))
        XCTAssertEqual(
            VersionTuple.parse("2.1.0"),
            VersionTuple(major: 2, minor: 1, patch: 0)
        )
    }

    func testVersionTupleDescription() {
        XCTAssertEqual(VersionTuple(major: 1, minor: 0, patch: 28).description, "1.0.28")
    }

    // MARK: Basic availability detection

    func testUpToDateWhenRunningLatestVersion() async {
        let checker = makeChecker(
            current: "1.0.28",
            releases: [release(tag: "boss-v1.0.28")]
        )
        let result = await checker.checkForUpdates()
        XCTAssertEqual(result, .upToDate)
    }

    func testUpToDateWhenNoReleasesMatch() async {
        let checker = makeChecker(current: "1.0.28", releases: [])
        let result = await checker.checkForUpdates()
        XCTAssertEqual(result, .upToDate)
    }

    func testAvailableWhenSingleNewerReleaseExists() async {
        let checker = makeChecker(
            current: "1.0.27",
            releases: [release(tag: "boss-v1.0.28")]
        )
        let result = await checker.checkForUpdates()
        guard case .available(let update) = result else {
            return XCTFail("expected .available, got \(result)")
        }
        XCTAssertEqual(update.tagName, "boss-v1.0.28")
        XCTAssertEqual(update.version, VersionTuple(major: 1, minor: 0, patch: 28))
        XCTAssertEqual(
            update.assetURL.absoluteString,
            "https://github.com/spinyfin/mono/releases/download/boss-v1.0.28/Boss-1.0.28.zip"
        )
        XCTAssertEqual(update.assetSize, 34_000_000)
        XCTAssertEqual(update.releaseNotes, "Release notes for 1.0.28")
    }

    // MARK: Maximum-tuple selection from interleaved list

    /// Live observation: `/releases` sorts by publish date, not version number, producing
    /// orderings like 1.0.27, 1.0.26, …, 1.0.9, 1.0.8, 1.0.18, 1.0.17 where the patch
    /// numbers are not monotone. The checker must pick the *maximum* tuple, not the first.
    func testPicksMaxVersionFromInterleavedList() async {
        // Matches the interleaved ordering observed in the live repo.
        let checker = makeChecker(
            current: "1.0.17",
            releases: [
                release(tag: "boss-v1.0.27"),
                release(tag: "boss-v1.0.26"),
                release(tag: "boss-v1.0.9"),
                release(tag: "boss-v1.0.8"),
                release(tag: "boss-v1.0.18"),
                release(tag: "boss-v1.0.17"),
            ]
        )
        let result = await checker.checkForUpdates()
        guard case .available(let update) = result else {
            return XCTFail("expected .available, got \(result)")
        }
        XCTAssertEqual(update.tagName, "boss-v1.0.27", "must pick the maximum, not the first entry")
    }

    // MARK: Assetless-release skipping

    /// Live observation: `boss-v1.0.21` exists in the repo with no `Boss-1.0.21.zip` asset.
    /// The checker must skip it and pick the next-best release that does have an asset.
    func testSkipsAssetlessReleaseAndPicksNextBest() async {
        let checker = makeChecker(
            current: "1.0.19",
            releases: [
                // highest version, but no asset
                releaseWithoutAsset(tag: "boss-v1.0.21"),
                release(tag: "boss-v1.0.20"),
            ]
        )
        let result = await checker.checkForUpdates()
        guard case .available(let update) = result else {
            return XCTFail("expected .available for 1.0.20, got \(result)")
        }
        XCTAssertEqual(update.tagName, "boss-v1.0.20")
    }

    func testUpToDateWhenAllReleasesLackAssets() async {
        let checker = makeChecker(
            current: "1.0.19",
            releases: [
                releaseWithoutAsset(tag: "boss-v1.0.21"),
                releaseWithoutAsset(tag: "boss-v1.0.20"),
            ]
        )
        let result = await checker.checkForUpdates()
        XCTAssertEqual(result, .upToDate)
    }

    // MARK: Changelog aggregation

    func testChangelogContainsSingleVersionWhenOneReleaseAhead() async {
        let checker = makeChecker(
            current: "1.0.27",
            releases: [release(tag: "boss-v1.0.28")]
        )
        let result = await checker.checkForUpdates()
        guard case .available(let update) = result else {
            return XCTFail("expected .available, got \(result)")
        }
        XCTAssertEqual(update.changelog.count, 1)
        XCTAssertEqual(update.changelog[0].version, VersionTuple(major: 1, minor: 0, patch: 28))
        XCTAssertEqual(update.changelog[0].notes, "Release notes for 1.0.28")
    }

    func testChangelogContainsAllVersionsInRange() async {
        // Installed: 1.0.25 — available: 1.0.26, 1.0.27, 1.0.28 (newest first in changelog)
        let checker = makeChecker(
            current: "1.0.25",
            releases: [
                release(tag: "boss-v1.0.28"),
                release(tag: "boss-v1.0.27"),
                release(tag: "boss-v1.0.26"),
                release(tag: "boss-v1.0.25"),  // installed — must be excluded
                release(tag: "boss-v1.0.24"),  // older — must be excluded
            ]
        )
        let result = await checker.checkForUpdates()
        guard case .available(let update) = result else {
            return XCTFail("expected .available, got \(result)")
        }
        XCTAssertEqual(update.version, VersionTuple(major: 1, minor: 0, patch: 28))
        XCTAssertEqual(update.changelog.count, 3)
        XCTAssertEqual(update.changelog.map(\.version), [
            VersionTuple(major: 1, minor: 0, patch: 28),
            VersionTuple(major: 1, minor: 0, patch: 27),
            VersionTuple(major: 1, minor: 0, patch: 26),
        ], "changelog must be newest-first and exclude installed and older versions")
    }

    func testChangelogIncludesAssetlessIntermediateVersions() async {
        // boss-v1.0.21 has no zip asset but its notes should appear in the changelog
        // because it is in the range (1.0.19, 1.0.22].
        let checker = makeChecker(
            current: "1.0.19",
            releases: [
                release(tag: "boss-v1.0.22"),
                releaseWithoutAsset(tag: "boss-v1.0.21"),
                release(tag: "boss-v1.0.20"),
            ]
        )
        let result = await checker.checkForUpdates()
        guard case .available(let update) = result else {
            return XCTFail("expected .available, got \(result)")
        }
        XCTAssertEqual(update.version, VersionTuple(major: 1, minor: 0, patch: 22))
        XCTAssertEqual(update.changelog.count, 3,
            "assetless intermediate version should appear in the changelog")
        XCTAssertEqual(update.changelog.map(\.version), [
            VersionTuple(major: 1, minor: 0, patch: 22),
            VersionTuple(major: 1, minor: 0, patch: 21),
            VersionTuple(major: 1, minor: 0, patch: 20),
        ])
    }

    func testChangelogExcludesAssetlessVersionBeyondDownloadTarget() async {
        // boss-v1.0.21 has no asset and IS the highest version, so the download target
        // falls back to boss-v1.0.20. The changelog should only cover (1.0.19, 1.0.20].
        let checker = makeChecker(
            current: "1.0.19",
            releases: [
                releaseWithoutAsset(tag: "boss-v1.0.21"),
                release(tag: "boss-v1.0.20"),
            ]
        )
        let result = await checker.checkForUpdates()
        guard case .available(let update) = result else {
            return XCTFail("expected .available, got \(result)")
        }
        XCTAssertEqual(update.version, VersionTuple(major: 1, minor: 0, patch: 20))
        XCTAssertEqual(update.changelog.count, 1)
        XCTAssertEqual(update.changelog[0].version, VersionTuple(major: 1, minor: 0, patch: 20))
    }

    // MARK: Tag filtering

    func testIgnoresDraftReleases() async {
        let checker = makeChecker(
            current: "1.0.27",
            releases: [
                release(tag: "boss-v1.0.28", draft: true),
            ]
        )
        let result = await checker.checkForUpdates()
        XCTAssertEqual(result, .upToDate)
    }

    func testIgnoresPrereleaseReleases() async {
        let checker = makeChecker(
            current: "1.0.27",
            releases: [
                release(tag: "boss-v1.0.28", prerelease: true),
            ]
        )
        let result = await checker.checkForUpdates()
        XCTAssertEqual(result, .upToDate)
    }

    func testIgnoresNonBossTaggedReleases() async {
        let checker = makeChecker(
            current: "1.0.27",
            releases: [
                // checkleft-v* tags coexist in the monorepo; must be ignored.
                release(tag: "checkleft-v0.1.0-alpha.8"),
                release(tag: "boss-v1.0.28"),
            ]
        )
        let result = await checker.checkForUpdates()
        guard case .available(let update) = result else {
            return XCTFail("expected .available, got \(result)")
        }
        XCTAssertEqual(update.tagName, "boss-v1.0.28")
    }

    func testIgnoresBossTagsWithSuffix() async {
        // Tags like "boss-v1.0.28-rc1" do not match the strict pattern.
        let checker = makeChecker(
            current: "1.0.27",
            releases: [release(tag: "boss-v1.0.28-rc1")]
        )
        let result = await checker.checkForUpdates()
        XCTAssertEqual(result, .upToDate)
    }

    // MARK: Dev-build detection

    func testIsDevBuildTrueWhenFullVersionContainsDevSuffix() {
        let checker = UpdateChecker(
            currentVersionString: "1.0.27",
            fullVersionString: "1.0.27-dev-f3be785",
            fetcher: .live
        )
        XCTAssertTrue(checker.isDevBuild)
    }

    func testIsDevBuildFalseForReleaseVersion() {
        let checker = UpdateChecker(
            currentVersionString: "1.0.27",
            fullVersionString: "1.0.27",
            fetcher: .live
        )
        XCTAssertFalse(checker.isDevBuild)
    }

    /// Dev builds still report update availability — the consumer (UpdateModel) is
    /// responsible for suppressing auto-install.
    func testDevBuildStillReportsAvailableUpdate() async {
        let checker = makeChecker(
            current: "1.0.27",
            fullVersion: "1.0.27-dev-abc1234",
            releases: [release(tag: "boss-v1.0.28")]
        )
        let result = await checker.checkForUpdates()
        guard case .available = result else {
            return XCTFail("dev build should still surface update availability, got \(result)")
        }
    }

    // MARK: Request headers

    func testSetsRequiredRequestHeaders() async throws {
        let recorder = RequestRecorder()
        let fetcher = HTTPFetcher { request in
            recorder.record(request)
            return (self.jsonData([self.release(tag: "boss-v1.0.28")]), self.response200())
        }
        let checker = UpdateChecker(
            currentVersionString: "1.0.27",
            fullVersionString: "1.0.27",
            fetcher: fetcher
        )

        _ = await checker.checkForUpdates()

        let req = try XCTUnwrap(recorder.all.first)
        XCTAssertEqual(req.value(forHTTPHeaderField: "Accept"), "application/vnd.github+json")
        XCTAssertEqual(req.value(forHTTPHeaderField: "X-GitHub-Api-Version"), "2022-11-28")
        XCTAssertEqual(req.value(forHTTPHeaderField: "User-Agent"), "Boss/1.0.27")
    }

    // MARK: ETag / conditional requests

    func testStoresETagFromSuccessfulResponse() async {
        let recorder = RequestRecorder()
        let fetcher = HTTPFetcher { request in
            recorder.record(request)
            return (
                self.jsonData([self.release(tag: "boss-v1.0.28")]),
                self.response200(headers: ["ETag": "\"etag-abc\""])
            )
        }
        let checker = UpdateChecker(
            currentVersionString: "1.0.27",
            fullVersionString: "1.0.27",
            fetcher: fetcher
        )

        _ = await checker.checkForUpdates()
        _ = await checker.checkForUpdates()

        XCTAssertEqual(recorder.all.count, 2)
        // First request: no If-None-Match
        XCTAssertNil(
            recorder.all[0].value(forHTTPHeaderField: "If-None-Match"),
            "first request must not carry If-None-Match"
        )
        // Second request: carries the stored ETag
        XCTAssertEqual(
            recorder.all[1].value(forHTTPHeaderField: "If-None-Match"),
            "\"etag-abc\"",
            "second request must send stored ETag as If-None-Match"
        )
    }

    func testReturns304ResponseUsingCachedResult() async {
        let callCount = Counter()
        let fetcher = HTTPFetcher { _ in
            callCount.increment()
            if callCount.value == 1 {
                return (
                    self.jsonData([self.release(tag: "boss-v1.0.28")]),
                    self.response200(headers: ["ETag": "\"etag-xyz\""])
                )
            } else {
                // 304 Not Modified — no body
                return (Data(), self.makeHTTPResponse(statusCode: 304))
            }
        }
        let checker = UpdateChecker(
            currentVersionString: "1.0.27",
            fullVersionString: "1.0.27",
            fetcher: fetcher
        )

        let first = await checker.checkForUpdates()   // 200: parses and caches result
        let second = await checker.checkForUpdates()  // 304: returns cached result

        guard case .available(let u1) = first, case .available(let u2) = second else {
            return XCTFail("expected .available for both calls, got \(first) and \(second)")
        }
        XCTAssertEqual(u1.tagName, u2.tagName, "304 response must return the cached result")
    }

    // MARK: Rate limiting

    func testRateLimited403WithXRateLimitReset() async {
        let resetEpoch = Date(timeIntervalSinceNow: 1800).timeIntervalSince1970
        let checker = makeCheckerWithHTTPStatus(
            429,
            headers: ["X-RateLimit-Reset": "\(Int(resetEpoch))"],
            current: "1.0.27"
        )
        let result = await checker.checkForUpdates()
        guard case .rateLimited(let retryAfter) = result else {
            return XCTFail("expected .rateLimited, got \(result)")
        }
        XCTAssertGreaterThan(retryAfter, Date(), "retry-after must be in the future")
    }

    func testRateLimited429WithRetryAfterSeconds() async {
        let checker = makeCheckerWithHTTPStatus(
            429,
            headers: ["Retry-After": "120"],
            current: "1.0.27"
        )
        let result = await checker.checkForUpdates()
        guard case .rateLimited(let retryAfter) = result else {
            return XCTFail("expected .rateLimited, got \(result)")
        }
        XCTAssertGreaterThan(retryAfter, Date())
    }

    func testRateLimited403FallsBackToOneHourDefault() async {
        // No Retry-After or X-RateLimit-Reset headers → default 1-hour backoff.
        let checker = makeCheckerWithHTTPStatus(403, headers: [:], current: "1.0.27")
        let result = await checker.checkForUpdates()
        guard case .rateLimited(let retryAfter) = result else {
            return XCTFail("expected .rateLimited, got \(result)")
        }
        // Should be at least 50 minutes from now (allowing a little slack in CI).
        XCTAssertGreaterThan(retryAfter, Date(timeIntervalSinceNow: 3000))
    }

    // MARK: Network errors

    func testNetworkErrorPropagated() async {
        let fetcher = HTTPFetcher { _ in
            throw URLError(.notConnectedToInternet)
        }
        let checker = UpdateChecker(
            currentVersionString: "1.0.27",
            fullVersionString: "1.0.27",
            fetcher: fetcher
        )
        let result = await checker.checkForUpdates()
        guard case .networkError = result else {
            return XCTFail("expected .networkError, got \(result)")
        }
    }

    func testUnexpectedStatusCodeReturnsNetworkError() async {
        let checker = makeCheckerWithHTTPStatus(500, headers: [:], current: "1.0.27")
        let result = await checker.checkForUpdates()
        guard case .networkError(let msg) = result else {
            return XCTFail("expected .networkError, got \(result)")
        }
        XCTAssertTrue(msg.contains("500"))
    }
}

// MARK: - Test helpers

extension UpdateCheckerTests {

    /// Builds an `UpdateChecker` backed by a stub fetcher serving `releases` as JSON.
    private func makeChecker(
        current: String,
        fullVersion: String? = nil,
        releases: [[String: Any]],
        etag: String? = nil
    ) -> UpdateChecker {
        let data = jsonData(releases)
        var headersMut: [String: String] = [:]
        if let etag { headersMut["ETag"] = etag }
        let headers = headersMut
        let fetcher = HTTPFetcher { _ in
            (data, self.response200(headers: headers))
        }
        return UpdateChecker(
            currentVersionString: current,
            fullVersionString: fullVersion ?? current,
            fetcher: fetcher
        )
    }

    private func makeCheckerWithHTTPStatus(
        _ statusCode: Int,
        headers: [String: String],
        current: String
    ) -> UpdateChecker {
        let fetcher = HTTPFetcher { _ in
            (Data(), self.makeHTTPResponse(statusCode: statusCode, headers: headers))
        }
        return UpdateChecker(
            currentVersionString: current,
            fullVersionString: current,
            fetcher: fetcher
        )
    }

    // MARK: Canned JSON helpers

    private func release(
        tag: String,
        draft: Bool = false,
        prerelease: Bool = false,
        body: String? = nil,
        majorMinorPatch: (Int, Int, Int)? = nil
    ) -> [String: Any] {
        // Derive the expected asset name from the tag, falling back to explicit tuple.
        let assetName: String
        let assetURL: String
        if let (major, minor, patch) = majorMinorPatch {
            assetName = "Boss-\(major).\(minor).\(patch).zip"
            assetURL =
                "https://github.com/spinyfin/mono/releases/download/\(tag)/\(assetName)"
        } else {
            // Parse major.minor.patch from tag (boss-vX.Y.Z).
            let stripped = tag.dropFirst("boss-v".count)
            let parts = stripped.split(separator: ".")
            if parts.count == 3 {
                assetName = "Boss-\(parts[0]).\(parts[1]).\(parts[2]).zip"
                assetURL =
                    "https://github.com/spinyfin/mono/releases/download/\(tag)/\(assetName)"
            } else {
                // Non-boss tag or unexpected format — use a placeholder so the
                // tag-filter logic (not the asset filter) is what rejects it.
                assetName = "Unexpected.zip"
                assetURL = "https://example.com/Unexpected.zip"
            }
        }

        return [
            "tag_name": tag,
            "draft": draft,
            "prerelease": prerelease,
            "body": body ?? "Release notes for \(tag.dropFirst("boss-v".count))",
            "assets": [[
                "name": assetName,
                "size": 34_000_000,
                "browser_download_url": assetURL,
            ] as [String: Any]],
        ]
    }

    private func releaseWithoutAsset(tag: String) -> [String: Any] {
        [
            "tag_name": tag,
            "draft": false,
            "prerelease": false,
            "body": "Missing asset release",
            "assets": [] as [[String: Any]],
        ]
    }

    private func jsonData(_ releases: [[String: Any]]) -> Data {
        (try? JSONSerialization.data(withJSONObject: releases)) ?? Data()
    }

    // MARK: HTTPURLResponse helpers

    private func response200(headers: [String: String] = [:]) -> HTTPURLResponse {
        makeHTTPResponse(statusCode: 200, headers: headers)
    }

    private func makeHTTPResponse(
        statusCode: Int,
        headers: [String: String] = [:]
    ) -> HTTPURLResponse {
        HTTPURLResponse(
            url: UpdateChecker.releasesURL,
            statusCode: statusCode,
            httpVersion: "HTTP/1.1",
            headerFields: headers
        )!
    }
}

// MARK: - Request recorder (mirrors SignalRecorder in WorkerProcessKillerTests)

private final class RequestRecorder: @unchecked Sendable {
    private let lock = NSLock()
    private var storage: [URLRequest] = []

    var all: [URLRequest] {
        lock.lock()
        defer { lock.unlock() }
        return storage
    }

    func record(_ request: URLRequest) {
        lock.lock()
        storage.append(request)
        lock.unlock()
    }
}

// MARK: - Thread-safe call counter

private final class Counter: @unchecked Sendable {
    private let lock = NSLock()
    private var count = 0

    var value: Int {
        lock.lock()
        defer { lock.unlock() }
        return count
    }

    func increment() {
        lock.lock()
        count += 1
        lock.unlock()
    }
}
