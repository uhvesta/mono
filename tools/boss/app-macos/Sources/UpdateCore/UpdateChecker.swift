import Foundation

// MARK: - Version types

struct VersionTuple: Comparable, Equatable, Sendable, CustomStringConvertible {
    let major: Int
    let minor: Int
    let patch: Int

    var description: String { "\(major).\(minor).\(patch)" }

    static func parse(_ string: String) -> VersionTuple? {
        let parts = string.split(separator: ".", maxSplits: 2, omittingEmptySubsequences: false)
        guard parts.count == 3,
              let major = Int(parts[0]),
              let minor = Int(parts[1]),
              let patch = Int(parts[2]) else { return nil }
        return VersionTuple(major: major, minor: minor, patch: patch)
    }

    static func < (lhs: VersionTuple, rhs: VersionTuple) -> Bool {
        if lhs.major != rhs.major { return lhs.major < rhs.major }
        if lhs.minor != rhs.minor { return lhs.minor < rhs.minor }
        return lhs.patch < rhs.patch
    }
}

struct AvailableUpdate: Equatable, Sendable {
    let tagName: String
    let version: VersionTuple
    let assetURL: URL
    let assetSize: Int
    let releaseNotes: String
}

enum UpdateCheckResult: Equatable, Sendable {
    case upToDate
    case available(AvailableUpdate)
    /// Polling must suspend until `retryAfter`.
    case rateLimited(retryAfter: Date)
    case networkError(String)
}

// MARK: - HTTP abstraction (injectable for tests)

struct HTTPFetcher: Sendable {
    let fetch: @Sendable (URLRequest) async throws -> (Data, HTTPURLResponse)

    static let live = HTTPFetcher { request in
        let (data, response) = try await URLSession.shared.data(for: request)
        guard let httpResponse = response as? HTTPURLResponse else {
            throw URLError(.badServerResponse)
        }
        return (data, httpResponse)
    }
}

// MARK: - GitHub API response types

private struct GitHubRelease: Decodable, Sendable {
    let tagName: String
    let draft: Bool
    let prerelease: Bool
    let body: String?
    let assets: [GitHubAsset]

    enum CodingKeys: String, CodingKey {
        case tagName = "tag_name"
        case draft
        case prerelease
        case body
        case assets
    }
}

private struct GitHubAsset: Decodable, Sendable {
    let name: String
    let size: Int
    let browserDownloadURL: String

    enum CodingKeys: String, CodingKey {
        case name
        case size
        case browserDownloadURL = "browser_download_url"
    }
}

// MARK: - UpdateChecker actor

/// Polls the unauthenticated GitHub Releases API for newer Boss releases.
///
/// Performs ETag conditional requests to stay within the 60 req/hr unauthenticated
/// rate limit, filters to `boss-v*` tags, skips assetless releases, picks the
/// maximum version tuple, and compares against the running bundle version.
actor UpdateChecker {

    /// True when `BossFullVersion` in the bundle contains `-dev-`; callers use
    /// this to suppress auto-install while still surfacing update availability.
    nonisolated let isDevBuild: Bool

    private let currentVersion: VersionTuple
    private let fetcher: HTTPFetcher
    private let userAgentVersion: String

    private var storedETag: String?
    private var lastResult: UpdateCheckResult = .upToDate

    static let releasesURL = URL(
        string: "https://api.github.com/repos/spinyfin/mono/releases?per_page=100"
    )!

    // Regex is not Sendable but is immutable after compilation; nonisolated(unsafe)
    // is correct here — we never mutate this value after first use.
    nonisolated(unsafe) private static let bossTagRegex =
        #/^boss-v(?<major>\d+)\.(?<minor>\d+)\.(?<patch>\d+)$/#

    init(
        currentVersionString: String,
        fullVersionString: String,
        fetcher: HTTPFetcher
    ) {
        self.currentVersion = VersionTuple.parse(currentVersionString)
            ?? VersionTuple(major: 0, minor: 0, patch: 0)
        self.isDevBuild = fullVersionString.contains("-dev-")
        self.userAgentVersion = currentVersionString
        self.fetcher = fetcher
    }

    /// Convenience initializer that reads version info from `Bundle.main`.
    static func fromBundle() -> UpdateChecker? {
        guard let info = Bundle.main.infoDictionary,
              let shortVersion = info["CFBundleShortVersionString"] as? String else { return nil }
        let fullVersion = info["BossFullVersion"] as? String ?? shortVersion
        return UpdateChecker(
            currentVersionString: shortVersion,
            fullVersionString: fullVersion,
            fetcher: .live
        )
    }

    func checkForUpdates() async -> UpdateCheckResult {
        var request = URLRequest(url: Self.releasesURL)
        request.setValue("application/vnd.github+json", forHTTPHeaderField: "Accept")
        request.setValue("2022-11-28", forHTTPHeaderField: "X-GitHub-Api-Version")
        request.setValue("Boss/\(userAgentVersion)", forHTTPHeaderField: "User-Agent")
        if let etag = storedETag {
            request.setValue(etag, forHTTPHeaderField: "If-None-Match")
        }

        let data: Data
        let response: HTTPURLResponse
        do {
            (data, response) = try await fetcher.fetch(request)
        } catch {
            return .networkError(error.localizedDescription)
        }

        switch response.statusCode {
        case 200:
            if let etag = response.value(forHTTPHeaderField: "ETag") {
                storedETag = etag
            }
            let result = parseReleases(from: data)
            lastResult = result
            return result

        case 304:
            // Conditional request hit — nothing changed since our last check.
            return lastResult

        case 403, 429:
            return .rateLimited(retryAfter: parseRateLimitReset(from: response))

        default:
            return .networkError("HTTP \(response.statusCode)")
        }
    }

    // MARK: - Private helpers

    private func parseReleases(from data: Data) -> UpdateCheckResult {
        let releases: [GitHubRelease]
        do {
            releases = try JSONDecoder().decode([GitHubRelease].self, from: data)
        } catch {
            return .networkError("JSON decode failed: \(error.localizedDescription)")
        }

        guard let (latestVersion, release, asset) = selectBestRelease(from: releases) else {
            return .upToDate
        }
        guard latestVersion > currentVersion else {
            return .upToDate
        }
        guard let assetURL = URL(string: asset.browserDownloadURL) else {
            return .networkError("Invalid asset URL: \(asset.browserDownloadURL)")
        }
        return .available(AvailableUpdate(
            tagName: release.tagName,
            version: latestVersion,
            assetURL: assetURL,
            assetSize: asset.size,
            releaseNotes: release.body ?? ""
        ))
    }

    /// Returns the highest-versioned `boss-v*` release that has a matching zip asset,
    /// skipping drafts, prereleases, and releases with no downloadable asset.
    private func selectBestRelease(
        from releases: [GitHubRelease]
    ) -> (VersionTuple, GitHubRelease, GitHubAsset)? {
        var best: (VersionTuple, GitHubRelease, GitHubAsset)?

        for release in releases {
            guard !release.draft, !release.prerelease else { continue }

            guard let match = try? Self.bossTagRegex.wholeMatch(in: release.tagName),
                  let major = Int(match.major),
                  let minor = Int(match.minor),
                  let patch = Int(match.patch) else { continue }

            let version = VersionTuple(major: major, minor: minor, patch: patch)
            let expectedAsset = "Boss-\(major).\(minor).\(patch).zip"
            guard let asset = release.assets.first(where: { $0.name == expectedAsset }) else {
                continue
            }

            if best == nil || version > best!.0 {
                best = (version, release, asset)
            }
        }

        return best
    }

    private func parseRateLimitReset(from response: HTTPURLResponse) -> Date {
        // Prefer Retry-After (seconds or HTTP-date) over X-RateLimit-Reset (epoch).
        if let retryAfter = response.value(forHTTPHeaderField: "Retry-After"),
           let seconds = TimeInterval(retryAfter) {
            return Date(timeIntervalSinceNow: seconds)
        }
        if let reset = response.value(forHTTPHeaderField: "X-RateLimit-Reset"),
           let epoch = TimeInterval(reset) {
            return Date(timeIntervalSince1970: epoch)
        }
        return Date(timeIntervalSinceNow: 3600)
    }
}
